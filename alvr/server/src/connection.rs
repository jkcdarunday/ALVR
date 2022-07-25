use crate::{
    buttons::BUTTON_PATH_FROM_ID,
    sockets::{self, ResponseSocket, SseSocket, WelcomeSocket},
    statistics::StatisticsManager,
    tracking::TrackingManager,
    AlvrButtonType_BUTTON_TYPE_BINARY, AlvrButtonType_BUTTON_TYPE_SCALAR, AlvrButtonValue,
    AlvrButtonValue__bindgen_ty_1, AlvrDeviceMotion, AlvrQuat, EyeFov, OculusHand, HAPTICS_SENDER,
    IS_ALIVE, RESTART_NOTIFIER, RUNTIME, SERVER_DATA_MANAGER, SHUTDOWN_NOTIFIER,
    STATISTICS_MANAGER, VIDEO_SENDER,
};
use alvr_audio::{AudioDevice, AudioDeviceType};
use alvr_common::{
    glam::{Quat, UVec2},
    once_cell::sync::Lazy,
    parking_lot::Mutex,
    prelude::*,
    HEAD_ID,
};
use alvr_events::{ButtonEvent, ButtonValue, EventType};
use alvr_session::{CodecType, FrameSize, OpenvrConfig};
use alvr_sockets::{
    spawn_cancelable, ClientListAction, ClientStatistics, RequestPacket, ResponsePacket, SsePacket,
    StreamConfigPacket, StreamSocketBuilder, Tracking, VideoStreamingCapabilities, AUDIO_STREAM,
    HAPTICS_STREAM, STATISTICS_STREAM, TRACKING_STREAM, VIDEO_STREAM,
};
use futures::future::BoxFuture;
use settings_schema::Switch;
use std::{
    collections::HashMap, future, net::IpAddr, process::Command, sync::Arc, thread, time::Duration,
};
use tokio::{
    sync::{mpsc as tmpsc, Notify},
    time,
};

#[cfg(windows)]
use alvr_session::{OpenvrPropValue, OpenvrPropertyKey};

const RETRY_CONNECT_MIN_INTERVAL: Duration = Duration::from_secs(1);

pub struct ClientConnectionData {
    sse_socket: SseSocket,
    capabilities: Option<VideoStreamingCapabilities>,
}

// Each element represent an active connection (possibly in standby)
pub static CLIENT_DATA: Lazy<Mutex<HashMap<String, ClientConnectionData>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
pub static STREAMING_CLIENT_HOSTNAME: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));
pub static STREAM_SHUTDOWN_NOTIFIER: Lazy<Notify> = Lazy::new(Notify::new);

fn align32(value: f32) -> u32 {
    ((value / 32.).floor() * 32.) as u32
}

fn mbits_to_bytes(value: u64) -> u32 {
    (value * 1024 * 1024 / 8) as u32
}

// Alternate connection trials with client on the local network and manual IPs
pub fn handshake_loop() -> IntResult {
    let mut welcome_socket = WelcomeSocket::new().map_err(to_int_e!())?;

    loop {
        check_interrupt!(IS_ALIVE.value());

        let mut manual_client_ips = HashMap::new();
        for (hostname, connection_info) in &SERVER_DATA_MANAGER.lock().session().client_connections
        {
            for ip in &connection_info.manual_ips {
                manual_client_ips.insert(*ip, hostname.clone());
            }
        }

        if connect(manual_client_ips).is_ok() {
            // allow to connect to all manual clients in rapid succession
            continue;
        }

        let discovery_config = SERVER_DATA_MANAGER
            .lock()
            .session()
            .to_settings()
            .connection
            .client_discovery
            .clone();
        if let Switch::Enabled(config) = discovery_config {
            let (client_hostname, client_ip) = match welcome_socket.recv_non_blocking() {
                Ok(pair) => pair,
                Err(e) => {
                    debug!("UDP handshake packet listening: {e}");

                    thread::sleep(RETRY_CONNECT_MIN_INTERVAL);

                    continue;
                }
            };

            let mut data_manager = SERVER_DATA_MANAGER.lock();

            data_manager
                .update_client_list(client_hostname.clone(), ClientListAction::AddIfMissing);

            if config.auto_trust_clients {
                data_manager.update_client_list(client_hostname.clone(), ClientListAction::Trust);
            }

            if let Some(connection_desc) = data_manager
                .session()
                .client_connections
                .get(&client_hostname)
            {
                // do not attempt connection if the client is already connected
                if connection_desc.trusted
                    && !CLIENT_DATA.lock().keys().any(|h| *h == client_hostname)
                {
                    match connect([(client_ip, client_hostname.clone())].into_iter().collect()) {
                        Ok(()) => continue,
                        // use error!(): usually errors should not happen here
                        Err(e) => error!("Handshake error for {client_hostname}: {e}"),
                    }
                }
            }
        }
    }
}

fn connect(mut client_ips: HashMap<IpAddr, String>) -> IntResult {
    let (client_ip, response_socket, sse_socket) =
        sockets::split_server_control_socket(client_ips.keys(), Arc::clone(&IS_ALIVE))?;

    // Safety: this never panics because client_ip is picked from client_ips keys
    let client_hostname = client_ips.remove(&client_ip).unwrap();

    CLIENT_DATA.lock().insert(
        client_hostname.clone(),
        ClientConnectionData {
            sse_socket,
            capabilities: None,
        },
    );

    thread::spawn(move || {
        if let Err(InterruptibleError::Other(e)) =
            response_loop(response_socket, &client_hostname, client_ip)
        {
            warn!("Connection error for {client_hostname}: {e}")
        }

        error!("removed client connection");

        CLIENT_DATA.lock().remove(&client_hostname);
    });

    Ok(())
}

fn response_loop(
    mut response_socket: ResponseSocket,
    client_hostname: &str,
    client_ip: IpAddr,
) -> IntResult {
    loop {
        check_interrupt!(IS_ALIVE.value());

        response_socket.poll(|message| match message {
            RequestPacket::Session => ResponsePacket::Session(
                serde_json::to_string(SERVER_DATA_MANAGER.lock().session()).unwrap(),
            ),
            RequestPacket::ClientCapabilities(client_capabilities) => {
                CLIENT_DATA
                    .lock()
                    .get_mut(client_hostname)
                    .unwrap()
                    .capabilities = client_capabilities;

                let client_hostname = client_hostname.to_owned();
                thread::spawn(move || {
                    // todo: call this conditionally
                    if let Err(InterruptibleError::Other(e)) =
                        request_video_streaming(&client_hostname)
                    {
                        error!("{e}");
                    }
                });

                ResponsePacket::Ok
            }
            RequestPacket::ClientStreamSocketReady => {
                error!("ClientStreamSocketReady");
                if STREAMING_CLIENT_HOSTNAME.lock().is_none() {
                    *STREAMING_CLIENT_HOSTNAME.lock() = Some(client_hostname.to_owned());

                    if let Some(runtime) = &*RUNTIME.lock() {
                        let client_hostname = client_hostname.to_owned();
                        runtime.spawn(async move {
                            tokio::select! {
                                _ = streaming_loop(&client_hostname, client_ip) => (),
                                _ = STREAM_SHUTDOWN_NOTIFIER.notified() => (),
                                _ = SHUTDOWN_NOTIFIER.notified() => (),
                            }
                        });
                    }
                }

                ResponsePacket::Ok
            }
            RequestPacket::PlayspaceSync(area) => {
                // use a separate thread because SetChaperone() is blocking
                thread::spawn(move || {
                    let width = f32::max(area.x, 2.0);
                    let height = f32::max(area.y, 2.0);
                    unsafe { crate::SetChaperone(width, height) };
                });

                ResponsePacket::Ok
            }
            RequestPacket::RequestIdr => {
                unsafe { crate::RequestIDR() };

                ResponsePacket::Ok
            }
            RequestPacket::KeepAlive => ResponsePacket::Ok,
            RequestPacket::ViewsConfig(config) => unsafe {
                crate::SetViewsConfig(crate::ViewsConfigData {
                    fov: [
                        EyeFov {
                            left: config.fov[0].left,
                            right: config.fov[0].right,
                            top: config.fov[0].top,
                            bottom: config.fov[0].bottom,
                        },
                        EyeFov {
                            left: config.fov[1].left,
                            right: config.fov[1].right,
                            top: config.fov[1].top,
                            bottom: config.fov[1].bottom,
                        },
                    ],
                    ipd_m: config.ipd_m,
                });

                ResponsePacket::Ok
            },
            RequestPacket::Battery(packet) => {
                unsafe {
                    crate::SetBattery(packet.device_id, packet.gauge_value, packet.is_plugged)
                };

                if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                    stats.report_battery(packet.device_id, packet.gauge_value);
                }

                ResponsePacket::Ok
            }
            RequestPacket::VideoErrorReport => {
                unsafe { crate::VideoErrorReportReceive() };

                ResponsePacket::Ok
            }
            RequestPacket::Button { path_id, value } => {
                if SERVER_DATA_MANAGER
                    .lock()
                    .session()
                    .session_settings
                    .extra
                    .log_button_presses
                {
                    alvr_events::send_event(EventType::Button(ButtonEvent {
                        path: BUTTON_PATH_FROM_ID
                            .get(&path_id)
                            .cloned()
                            .unwrap_or_else(|| format!("Unknown (ID: {:#16x})", path_id)),
                        value: value.clone(),
                    }));
                }

                let value = match value {
                    ButtonValue::Binary(value) => AlvrButtonValue {
                        type_: AlvrButtonType_BUTTON_TYPE_BINARY,
                        __bindgen_anon_1: AlvrButtonValue__bindgen_ty_1 { binary: value },
                    },

                    ButtonValue::Scalar(value) => AlvrButtonValue {
                        type_: AlvrButtonType_BUTTON_TYPE_SCALAR,
                        __bindgen_anon_1: AlvrButtonValue__bindgen_ty_1 { scalar: value },
                    },
                };

                unsafe { crate::SetButton(path_id, value) };

                ResponsePacket::Ok
            }
            RequestPacket::ActiveInteractionProfile {
                device_id,
                profile_id,
            } => ResponsePacket::Err("unimplemented".into()),
            RequestPacket::Custom(_) => ResponsePacket::Custom("{}".into()),

            RequestPacket::StoreSession(_)
            | RequestPacket::StoreSettings(_)
            | RequestPacket::RegisterDriver
            | RequestPacket::UnregisterDriver
            | RequestPacket::RegisteredDrivers
            | RequestPacket::AudioDevices
            | RequestPacket::GraphicsDevice
            | RequestPacket::RestartSteamvr
            | RequestPacket::ClientListAction(_)
            | RequestPacket::ServerVersion
            | RequestPacket::ServerOs
            | RequestPacket::UpdateServer { .. } => ResponsePacket::Err("unimplemented".into()),
        })?;
    }
}

fn request_video_streaming(client_hostname: &str) -> IntResult {
    if let Some(connection_data) = CLIENT_DATA.lock().get_mut(client_hostname) {
        if let Some(video_capabilities) = &connection_data.capabilities {
            error!("got client capabilities");
            let mut data_manager = SERVER_DATA_MANAGER.lock();

            let settings = data_manager.session().to_settings();

            let (eye_width, eye_height) = match settings.video.render_resolution {
                FrameSize::Scale(scale) => (
                    video_capabilities.default_view_resolution.x as f32 * scale,
                    video_capabilities.default_view_resolution.y as f32 * scale,
                ),
                FrameSize::Absolute { width, height } => (width as f32 / 2_f32, height as f32),
            };
            let video_eye_width = align32(eye_width);
            let video_eye_height = align32(eye_height);

            let (eye_width, eye_height) = match settings.video.recommended_target_resolution {
                FrameSize::Scale(scale) => (
                    video_capabilities.default_view_resolution.x as f32 * scale,
                    video_capabilities.default_view_resolution.y as f32 * scale,
                ),
                FrameSize::Absolute { width, height } => (width as f32 / 2_f32, height as f32),
            };
            let target_eye_width = align32(eye_width);
            let target_eye_height = align32(eye_height);

            let fps = {
                let mut best_match = 0_f32;
                let mut min_diff = f32::MAX;
                for rr in &video_capabilities.supported_refresh_rates {
                    let diff = (*rr - settings.video.preferred_fps).abs();
                    if diff < min_diff {
                        best_match = *rr;
                        min_diff = diff;
                    }
                }
                best_match
            };

            if !video_capabilities
                .supported_refresh_rates
                .contains(&settings.video.preferred_fps)
            {
                warn!("Chosen refresh rate not supported. Using {fps}Hz");
            }

            let game_audio_sample_rate =
                if let Switch::Enabled(game_audio_desc) = settings.audio.game_audio {
                    let game_audio_device = AudioDevice::new(
                        Some(settings.audio.linux_backend),
                        game_audio_desc.device_id,
                        AudioDeviceType::Output,
                    )
                    .map_err(to_int_e!())?;

                    if let Switch::Enabled(microphone_desc) = settings.audio.microphone {
                        let microphone_device = AudioDevice::new(
                            Some(settings.audio.linux_backend),
                            microphone_desc.input_device_id,
                            AudioDeviceType::VirtualMicrophoneInput,
                        )
                        .map_err(to_int_e!())?;
                        #[cfg(not(target_os = "linux"))]
                        if alvr_audio::is_same_device(&game_audio_device, &microphone_device) {
                            return int_fmt_e!(
                                "Game audio and microphone cannot point to the same device!"
                            );
                        }
                    }

                    game_audio_device.input_sample_rate().map_err(to_int_e!())?
                } else {
                    0
                };

            let session_settings = data_manager.session().session_settings.clone();

            let new_openvr_config = OpenvrConfig {
                universe_id: settings.headset.universe_id,
                headset_serial_number: settings.headset.serial_number,
                headset_tracking_system_name: settings.headset.tracking_system_name,
                headset_model_number: settings.headset.model_number,
                headset_driver_version: settings.headset.driver_version,
                headset_manufacturer_name: settings.headset.manufacturer_name,
                headset_render_model_name: settings.headset.render_model_name,
                headset_registered_device_type: settings.headset.registered_device_type,
                eye_resolution_width: video_eye_width,
                eye_resolution_height: video_eye_height,
                target_eye_resolution_width: target_eye_width,
                target_eye_resolution_height: target_eye_height,
                seconds_from_vsync_to_photons: settings.video.seconds_from_vsync_to_photons,
                force_3dof: settings.headset.force_3dof,
                tracking_ref_only: settings.headset.tracking_ref_only,
                enable_vive_tracker_proxy: settings.headset.enable_vive_tracker_proxy,
                aggressive_keyframe_resend: settings.connection.aggressive_keyframe_resend,
                adapter_index: settings.video.adapter_index,
                codec: matches!(settings.video.codec, CodecType::HEVC) as _,
                refresh_rate: fps as _,
                use_10bit_encoder: settings.video.use_10bit_encoder,
                force_sw_encoding: settings.video.force_sw_encoding,
                sw_thread_count: settings.video.sw_thread_count,
                encode_bitrate_mbs: settings.video.encode_bitrate_mbs,
                enable_adaptive_bitrate: session_settings.video.adaptive_bitrate.enabled,
                bitrate_maximum: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .bitrate_maximum,
                latency_target: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .latency_target,
                latency_use_frametime: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .latency_use_frametime
                    .enabled,
                latency_target_maximum: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .latency_use_frametime
                    .content
                    .latency_target_maximum,
                latency_target_offset: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .latency_use_frametime
                    .content
                    .latency_target_offset,
                latency_threshold: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .latency_threshold,
                bitrate_up_rate: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .bitrate_up_rate,
                bitrate_down_rate: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .bitrate_down_rate,
                bitrate_light_load_threshold: session_settings
                    .video
                    .adaptive_bitrate
                    .content
                    .bitrate_light_load_threshold,
                controllers_tracking_system_name: session_settings
                    .headset
                    .controllers
                    .content
                    .tracking_system_name
                    .clone(),
                controllers_manufacturer_name: session_settings
                    .headset
                    .controllers
                    .content
                    .manufacturer_name
                    .clone(),
                controllers_model_number: session_settings
                    .headset
                    .controllers
                    .content
                    .model_number
                    .clone(),
                render_model_name_left_controller: session_settings
                    .headset
                    .controllers
                    .content
                    .render_model_name_left
                    .clone(),
                render_model_name_right_controller: session_settings
                    .headset
                    .controllers
                    .content
                    .render_model_name_right
                    .clone(),
                controllers_serial_number: session_settings
                    .headset
                    .controllers
                    .content
                    .serial_number
                    .clone(),
                controllers_type_left: session_settings
                    .headset
                    .controllers
                    .content
                    .ctrl_type_left
                    .clone(),
                controllers_type_right: session_settings
                    .headset
                    .controllers
                    .content
                    .ctrl_type_right
                    .clone(),
                controllers_registered_device_type: session_settings
                    .headset
                    .controllers
                    .content
                    .registered_device_type
                    .clone(),
                controllers_input_profile_path: session_settings
                    .headset
                    .controllers
                    .content
                    .input_profile_path
                    .clone(),
                controllers_mode_idx: session_settings.headset.controllers.content.mode_idx,
                controllers_enabled: session_settings.headset.controllers.enabled,
                position_offset: settings.headset.position_offset,
                linear_velocity_cutoff: session_settings
                    .headset
                    .controllers
                    .content
                    .linear_velocity_cutoff,
                angular_velocity_cutoff: session_settings
                    .headset
                    .controllers
                    .content
                    .angular_velocity_cutoff,
                position_offset_left: session_settings
                    .headset
                    .controllers
                    .content
                    .position_offset_left,
                rotation_offset_left: session_settings
                    .headset
                    .controllers
                    .content
                    .rotation_offset_left,
                haptics_intensity: session_settings
                    .headset
                    .controllers
                    .content
                    .haptics_intensity,
                haptics_amplitude_curve: session_settings
                    .headset
                    .controllers
                    .content
                    .haptics_amplitude_curve,
                haptics_min_duration: session_settings
                    .headset
                    .controllers
                    .content
                    .haptics_min_duration,
                haptics_low_duration_amplitude_multiplier: session_settings
                    .headset
                    .controllers
                    .content
                    .haptics_low_duration_amplitude_multiplier,
                haptics_low_duration_range: session_settings
                    .headset
                    .controllers
                    .content
                    .haptics_low_duration_range,
                use_headset_tracking_system: session_settings
                    .headset
                    .controllers
                    .content
                    .use_headset_tracking_system,
                enable_foveated_rendering: session_settings.video.foveated_rendering.enabled,
                foveation_center_size_x: session_settings
                    .video
                    .foveated_rendering
                    .content
                    .center_size_x,
                foveation_center_size_y: session_settings
                    .video
                    .foveated_rendering
                    .content
                    .center_size_y,
                foveation_center_shift_x: session_settings
                    .video
                    .foveated_rendering
                    .content
                    .center_shift_x,
                foveation_center_shift_y: session_settings
                    .video
                    .foveated_rendering
                    .content
                    .center_shift_y,
                foveation_edge_ratio_x: session_settings
                    .video
                    .foveated_rendering
                    .content
                    .edge_ratio_x,
                foveation_edge_ratio_y: session_settings
                    .video
                    .foveated_rendering
                    .content
                    .edge_ratio_y,
                enable_color_correction: session_settings.video.color_correction.enabled,
                brightness: session_settings.video.color_correction.content.brightness,
                contrast: session_settings.video.color_correction.content.contrast,
                saturation: session_settings.video.color_correction.content.saturation,
                gamma: session_settings.video.color_correction.content.gamma,
                sharpening: session_settings.video.color_correction.content.sharpening,
                enable_fec: session_settings.connection.enable_fec,
                linux_async_reprojection: session_settings.extra.patches.linux_async_reprojection,
            };

            if data_manager.session().openvr_config != new_openvr_config {
                data_manager.session_mut().openvr_config = new_openvr_config;

                connection_data
                    .sse_socket
                    .send(SsePacket::ServerRestarting)
                    .ok();

                crate::notify_restart_driver();

                interrupt()
            } else {
                error!("start streaming event");
                connection_data
                    .sse_socket
                    .send(SsePacket::StartStreaming(StreamConfigPacket {
                        view_resolution: UVec2::new(video_eye_width, video_eye_height),
                        fps,
                        game_audio_sample_rate,
                    }))
            }
        } else {
            int_fmt_e!("Client does not support video streaming: {client_hostname}")
        }
    } else {
        int_fmt_e!("No such client: {client_hostname}")
    }
}

// close stream on Drop (manual disconnection or execution canceling)
struct StreamCloseGuard;

impl Drop for StreamCloseGuard {
    fn drop(&mut self) {
        unsafe { crate::DeinitializeStreaming() };

        let settings = SERVER_DATA_MANAGER.lock().session().to_settings();

        let on_disconnect_script = settings.connection.on_disconnect_script;
        if !on_disconnect_script.is_empty() {
            info!("Running on disconnect script (disconnect): {on_disconnect_script}");
            if let Err(e) = Command::new(&on_disconnect_script)
                .env("ACTION", "disconnect")
                .spawn()
            {
                warn!("Failed to run disconnect script: {e}");
            }
        }

        *STREAMING_CLIENT_HOSTNAME.lock() = None;
    }
}

async fn streaming_loop(client_hostname: &str, client_ip: IpAddr) -> StrResult {
    error!("streaming_loop 1");

    let settings = SERVER_DATA_MANAGER.lock().session().to_settings();

    let stream_socket = tokio::select! {
        res = StreamSocketBuilder::connect_to_client(
            client_ip,
            settings.connection.stream_port,
            settings.connection.stream_protocol,
            mbits_to_bytes(settings.video.encode_bitrate_mbs)
        ) => res?,
        _ = time::sleep(Duration::from_secs(5)) => {
            return fmt_e!("Timeout while setting up streams");
        }
    };
    let stream_socket = Arc::new(stream_socket);

    *STATISTICS_MANAGER.lock() = Some(StatisticsManager::new(
        settings.connection.statistics_history_size as _,
    ));

    error!("streaming_loop 2");

    alvr_events::send_event(EventType::ClientConnected);

    {
        let on_connect_script = settings.connection.on_connect_script;

        if !on_connect_script.is_empty() {
            info!("Running on connect script (connect): {on_connect_script}");
            if let Err(e) = Command::new(&on_connect_script)
                .env("ACTION", "connect")
                .spawn()
            {
                warn!("Failed to run connect script: {e}");
            }
        }
    }

    unsafe { crate::InitializeStreaming() };
    let _stream_guard = StreamCloseGuard;

    error!("streaming_loop 2");

    let game_audio_loop: BoxFuture<_> = if let Switch::Enabled(desc) = settings.audio.game_audio {
        let device = AudioDevice::new(
            Some(settings.audio.linux_backend),
            desc.device_id,
            AudioDeviceType::Output,
        )?;
        let sender = stream_socket.request_stream(AUDIO_STREAM).await?;
        let mute_when_streaming = desc.mute_when_streaming;

        Box::pin(async move {
            #[cfg(windows)]
            unsafe {
                let device_id = alvr_audio::get_windows_device_id(&device)?;
                crate::SetOpenvrProperty(
                    *HEAD_ID,
                    crate::to_cpp_openvr_prop(
                        OpenvrPropertyKey::AudioDefaultPlaybackDeviceId,
                        OpenvrPropValue::String(device_id),
                    ),
                )
            }

            alvr_audio::record_audio_loop(device, 2, mute_when_streaming, sender).await?;

            #[cfg(windows)]
            {
                let default_device = AudioDevice::new(
                    None,
                    alvr_session::AudioDeviceId::Default,
                    AudioDeviceType::Output,
                )?;
                let default_device_id = alvr_audio::get_windows_device_id(&default_device)?;

                unsafe {
                    crate::SetOpenvrProperty(
                        *HEAD_ID,
                        crate::to_cpp_openvr_prop(
                            OpenvrPropertyKey::AudioDefaultPlaybackDeviceId,
                            OpenvrPropValue::String(default_device_id),
                        ),
                    )
                }
            }

            Ok(())
        })
    } else {
        Box::pin(future::pending())
    };

    let microphone_loop: BoxFuture<_> = if let Switch::Enabled(desc) = settings.audio.microphone {
        let input_device = AudioDevice::new(
            Some(settings.audio.linux_backend),
            desc.input_device_id,
            AudioDeviceType::VirtualMicrophoneInput,
        )?;
        let receiver = stream_socket.subscribe_to_stream(AUDIO_STREAM).await?;

        #[cfg(windows)]
        {
            let microphone_device = AudioDevice::new(
                None,
                desc.output_device_id,
                AudioDeviceType::VirtualMicrophoneOutput {
                    matching_input_device_name: input_device.name()?,
                },
            )?;
            let microphone_device_id = alvr_audio::get_windows_device_id(&microphone_device)?;
            unsafe {
                crate::SetOpenvrProperty(
                    *HEAD_ID,
                    crate::to_cpp_openvr_prop(
                        OpenvrPropertyKey::AudioDefaultRecordingDeviceId,
                        OpenvrPropValue::String(microphone_device_id),
                    ),
                )
            }
        }

        let microphone_sample_rate = CLIENT_DATA
            .lock()
            .get(client_hostname)
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap()
            .microphone_sample_rate;

        Box::pin(alvr_audio::play_audio_loop(
            input_device,
            1,
            microphone_sample_rate,
            desc.buffering_config,
            receiver,
        ))
    } else {
        Box::pin(future::pending())
    };

    let video_send_loop = {
        let mut socket_sender = stream_socket.request_stream(VIDEO_STREAM).await?;
        async move {
            let (data_sender, mut data_receiver) = tmpsc::unbounded_channel();
            *VIDEO_SENDER.lock() = Some(data_sender);

            while let Some((header, data)) = data_receiver.recv().await {
                let mut buffer = socket_sender.new_buffer(&header, data.len())?;
                buffer.get_mut().extend(data);
                socket_sender.send_buffer(buffer).await.ok();
            }

            Ok(())
        }
    };

    let haptics_send_loop = {
        let mut socket_sender = stream_socket.request_stream(HAPTICS_STREAM).await?;
        async move {
            let (data_sender, mut data_receiver) = tmpsc::unbounded_channel();
            *HAPTICS_SENDER.lock() = Some(data_sender);

            while let Some(haptics) = data_receiver.recv().await {
                socket_sender
                    .send_buffer(socket_sender.new_buffer(&haptics, 0)?)
                    .await
                    .ok();
            }

            Ok(())
        }
    };

    fn to_tracking_quat(quat: Quat) -> AlvrQuat {
        AlvrQuat {
            x: quat.x,
            y: quat.y,
            z: quat.z,
            w: quat.w,
        }
    }

    let tracking_receive_loop = {
        let mut receiver = stream_socket
            .subscribe_to_stream::<Tracking>(TRACKING_STREAM)
            .await?;
        async move {
            let controller_prediction_multiplier = settings
                .headset
                .controllers
                .clone()
                .into_option()
                .map(|c| c.prediction_multiplier)
                .unwrap_or_default();
            let tracking_manager = TrackingManager::new(settings.headset);
            loop {
                let tracking = receiver.recv().await?.header;

                let mut device_motions = vec![];
                for (id, motion) in tracking.device_motions {
                    let motion = if id == *HEAD_ID {
                        tracking_manager.map_head(motion)
                    } else if let Some(motion) = tracking_manager.map_controller(motion) {
                        motion
                    } else {
                        continue;
                    };
                    device_motions.push((id, motion));
                }

                let raw_motions = device_motions
                    .into_iter()
                    .map(|(id, motion)| AlvrDeviceMotion {
                        deviceID: id,
                        orientation: to_tracking_quat(motion.orientation),
                        position: motion.position.to_array(),
                        linearVelocity: motion.linear_velocity.to_array(),
                        angularVelocity: motion.angular_velocity.to_array(),
                    })
                    .collect::<Vec<_>>();

                let left_oculus_hand = if let Some(arr) = tracking.left_hand_skeleton {
                    let vec = arr.into_iter().map(to_tracking_quat).collect::<Vec<_>>();
                    let mut array = [AlvrQuat::default(); 19];
                    array.copy_from_slice(&vec);

                    OculusHand {
                        enabled: true,
                        boneRotations: array,
                    }
                } else {
                    OculusHand {
                        enabled: false,
                        ..Default::default()
                    }
                };

                let right_oculus_hand = if let Some(arr) = tracking.right_hand_skeleton {
                    let vec = arr.into_iter().map(to_tracking_quat).collect::<Vec<_>>();
                    let mut array = [AlvrQuat::default(); 19];
                    array.copy_from_slice(&vec);

                    OculusHand {
                        enabled: true,
                        boneRotations: array,
                    }
                } else {
                    OculusHand {
                        enabled: false,
                        ..Default::default()
                    }
                };

                if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                    stats.report_tracking_received(tracking.target_timestamp);

                    let prediction_s = stats.average_total_latency().as_secs_f32()
                        * controller_prediction_multiplier;

                    unsafe {
                        crate::SetTracking(
                            tracking.target_timestamp.as_nanos() as _,
                            prediction_s,
                            raw_motions.as_ptr(),
                            raw_motions.len() as _,
                            left_oculus_hand,
                            right_oculus_hand,
                        )
                    };
                }
            }
        }
    };

    let statistics_receive_loop = {
        let mut receiver = stream_socket
            .subscribe_to_stream::<ClientStatistics>(STATISTICS_STREAM)
            .await?;
        async move {
            loop {
                let client_stats = receiver.recv().await?.header;

                if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                    let game_frame_interval =
                        Duration::from_nanos(unsafe { crate::GetGameFrameIntervalNs() });
                    let network_latency =
                        stats.report_statistics(client_stats, game_frame_interval);
                    unsafe { crate::ReportNetworkLatency(network_latency.as_micros() as _) };
                }
            }
        }
    };

    let receive_loop = async move { stream_socket.receive_loop().await };

    error!("streaming_loop 3");

    tokio::select! {
        // Spawn new tasks and let the runtime manage threading
        res = spawn_cancelable(receive_loop) => {
            alvr_events::send_event(EventType::ClientDisconnected);
            if let Err(e) = res {
                info!("Client disconnected. Cause: {e}" );
            }

            Ok(())
        },
        res = spawn_cancelable(game_audio_loop) => res,
        res = spawn_cancelable(microphone_loop) => res,
        res = spawn_cancelable(video_send_loop) => res,
        res = spawn_cancelable(statistics_receive_loop) => res,
        res = spawn_cancelable(haptics_send_loop) => res,
        res = spawn_cancelable(tracking_receive_loop) => res,

        _ = RESTART_NOTIFIER.notified() => {
            if let Some(data) = CLIENT_DATA.lock().get_mut(client_hostname) {
                data.sse_socket.send(SsePacket::ServerRestarting).ok();
            }

            Ok(())
        }
    }
}
