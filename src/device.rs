use data_url::DataUrl;
use image::load_from_memory_with_format;
use mirajazz::{device::Device, error::MirajazzError, state::DeviceStateUpdate};
use openaction::{OUTBOUND_EVENT_MANAGER, SetImageEvent};
use tokio_util::sync::CancellationToken;

use crate::{
    DEVICES, TOKENS,
    inputs::opendeck_to_device,
    mappings::{
        COL_COUNT, CandidateDevice, ENCODER_COUNT, KEY_COUNT, Kind, ROW_COUNT,
        get_image_format_for_key,
    },
};

/// Initializes a device and listens for events
pub async fn device_task(candidate: CandidateDevice, token: CancellationToken) {
    log::info!("Running device task for {:?}", candidate);

    // Wrap in a closure so we can use `?` operator
    let device = async || -> Result<Device, MirajazzError> {
        log::info!("Connecting to device...");
        let device = connect(&candidate).await?;
        log::info!("Device connected successfully");

        // Try to set brightness - some devices may not support this command
        log::info!("Setting brightness...");
        if let Err(e) = device.set_brightness(50).await {
            log::warn!("Failed to set brightness (this may be normal for this device): {}", e);
            // Continue anyway - brightness setting might not be supported
        } else {
            log::info!("Brightness set successfully");
        }

        // Try to clear all button images - some devices may not support this command
        log::info!("Clearing all button images...");
        if let Err(e) = device.clear_all_button_images().await {
            log::warn!("Failed to clear all button images (this may be normal for this device): {}", e);
            // Continue anyway - clearing might not be supported or needed
        } else {
            log::info!("Button images cleared successfully");
        }

        // Try to flush - some devices may not need this
        log::info!("Flushing device...");
        if let Err(e) = device.flush().await {
            log::warn!("Failed to flush device (this may be normal for this device): {}", e);
            // Continue anyway
        } else {
            log::info!("Device flushed successfully");
        }

        Ok(device)
    }()
    .await;

    let device: Device = match device {
        Ok(device) => device,
        Err(err) => {
            handle_error(&candidate.id, err).await;

            log::error!(
                "Had error during device init, finishing device task: {:?}",
                candidate
            );

            return;
        }
    };

    log::info!("Registering device {}", candidate.id);
    if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
        outbound
            .register_device(
                candidate.id.clone(),
                candidate.kind.human_name(),
                ROW_COUNT as u8,
                COL_COUNT as u8,
                ENCODER_COUNT as u8,
                0,
            )
            .await
            .unwrap();
    }

    DEVICES.write().await.insert(candidate.id.clone(), device);

    tokio::select! {
        _ = device_events_task(&candidate) => {},
        _ = token.cancelled() => {}
    };

    log::info!("Shutting down device {:?}", candidate);

    if let Some(device) = DEVICES.read().await.get(&candidate.id) {
        device.shutdown().await.ok();
    }

    log::info!("Device task finished for {:?}", candidate);
}

/// Handles errors, returning true if should continue, returning false if an error is fatal
pub async fn handle_error(id: &String, err: MirajazzError) -> bool {
    log::error!("Device {} error: {}", id, err);

    // Some errors are not critical and can be ignored without sending disconnected event
    if matches!(err, MirajazzError::ImageError(_) | MirajazzError::BadData) {
        return true;
    }

    log::info!("Deregistering device {}", id);
    if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
        outbound.deregister_device(id.clone()).await.unwrap();
    }

    log::info!("Cancelling tasks for device {}", id);
    if let Some(token) = TOKENS.read().await.get(id) {
        token.cancel();
    }

    log::info!("Removing device {} from the list", id);
    DEVICES.write().await.remove(id);

    log::info!("Finished clean-up for {}", id);

    false
}

pub async fn connect(candidate: &CandidateDevice) -> Result<Device, MirajazzError> {
    let result = Device::connect(
        &candidate.dev,
        candidate.kind.protocol_version(),
        KEY_COUNT,
        ENCODER_COUNT,
    )
    .await;

    match result {
        Ok(device) => Ok(device),
        Err(e) => {
            log::error!("Error while connecting to device: {e}");

            Err(e)
        }
    }
}

/// Handles events from device to OpenDeck
async fn device_events_task(candidate: &CandidateDevice) -> Result<(), MirajazzError> {
    log::info!("Connecting to {} for incoming events", candidate.id);

    let devices_lock = DEVICES.read().await;
    let reader = match devices_lock.get(&candidate.id) {
        Some(device) => device.get_reader(crate::inputs::process_input),
        None => return Ok(()),
    };
    drop(devices_lock);

    log::info!("Connected to {} for incoming events", candidate.id);

    log::info!("Reader is ready for {}", candidate.id);

    // Track last processed event to avoid duplicates
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    
    #[derive(Hash, PartialEq, Eq, Clone, Copy)]
    enum EventKey {
        ButtonDown(u8),
        ButtonUp(u8),
        EncoderDown(u8),
        EncoderUp(u8),
        EncoderTwist(u8, i16),
    }
    
    let mut last_events: HashSet<(EventKey, Instant)> = HashSet::new();
    let dedup_window = Duration::from_millis(500); // 500ms window for deduplication

    loop {
        log::info!("Reading updates...");

        let updates = match reader.read(None).await {
            Ok(updates) => updates,
            Err(e) => {
                if !handle_error(&candidate.id, e).await {
                    break;
                }

                continue;
            }
        };

        // Clean up old events from deduplication cache
        let now = Instant::now();
        last_events.retain(|(_, time)| now.duration_since(*time) < dedup_window);

        for update in updates {
            log::info!("New update: {:#?}", update);

            // Create a key for deduplication
            let event_key = match &update {
                DeviceStateUpdate::ButtonDown(key) => EventKey::ButtonDown(*key),
                DeviceStateUpdate::ButtonUp(key) => EventKey::ButtonUp(*key),
                DeviceStateUpdate::EncoderDown(enc) => EventKey::EncoderDown(*enc),
                DeviceStateUpdate::EncoderUp(enc) => EventKey::EncoderUp(*enc),
                DeviceStateUpdate::EncoderTwist(enc, val) => EventKey::EncoderTwist(*enc, *val as i16),
            };

            // Check for duplicates (same event type and key/encoder within the dedup window)
            let is_duplicate = last_events.iter().any(|(key, _)| *key == event_key);

            if is_duplicate {
                log::debug!("Skipping duplicate event: {:#?}", update);
                continue;
            }

            // Add to deduplication cache
            last_events.insert((event_key, now));

            let id = candidate.id.clone();

            if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
                match update {
                    DeviceStateUpdate::ButtonDown(key) => {
                        log::info!("Sending key_down event: device_id={}, key={}", id, key);
                        outbound.key_down(id.clone(), key).await.unwrap();
                    }
                    DeviceStateUpdate::ButtonUp(key) => {
                        log::info!("Sending key_up event: device_id={}, key={}", id, key);
                        outbound.key_up(id.clone(), key).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderDown(encoder) => {
                        outbound.encoder_down(id, encoder).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderUp(encoder) => {
                        outbound.encoder_up(id, encoder).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderTwist(encoder, val) => {
                        outbound
                            .encoder_change(id, encoder, val as i16)
                            .await
                            .unwrap();
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handles different combinations of "set image" event, including clearing the specific buttons and whole device
pub async fn handle_set_image(device: &Device, evt: SetImageEvent) -> Result<(), MirajazzError> {
    match (evt.position, evt.image) {
        (Some(position), Some(image)) => {
            log::info!("Setting image for button {}", position);

            // OpenDeck sends image as a data url, so parse it using a library
            let url = DataUrl::process(image.as_str()).unwrap(); // Isn't expected to fail, so unwrap it is
            let (body, _fragment) = url.decode_to_vec().unwrap(); // Same here

            // Allow only image/jpeg mime for now
            if url.mime_type().subtype != "jpeg" {
                log::error!("Incorrect mime type: {}", url.mime_type());

                return Ok(()); // Not a fatal error, enough to just log it
            }

            let image = load_from_memory_with_format(body.as_slice(), image::ImageFormat::Jpeg)?;

            let kind = Kind::from_vid_pid(device.vid, device.pid).unwrap(); // Safe to unwrap here, because device is already filtered

            device
                .set_button_image(
                    opendeck_to_device(position),
                    get_image_format_for_key(&kind, position),
                    image,
                )
                .await?;
            device.flush().await?;
        }
        (Some(position), None) => {
            device
                .clear_button_image(opendeck_to_device(position))
                .await?;
            device.flush().await?;
        }
        (None, None) => {
            device.clear_all_button_images().await?;
            device.flush().await?;
        }
        _ => {}
    }

    Ok(())
}
