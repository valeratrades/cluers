use base64::Engine;
use image::codecs::png::PngEncoder;
use image::{ColorType, GenericImageView, ImageEncoder, RgbaImage};
use serde::Deserialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};
use tokio::time::{sleep, Duration};
use xcap::Monitor;

#[derive(Debug, Deserialize)]
pub struct SelectionCoords {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Default)]
pub struct CaptureState {
    images: Mutex<Vec<RgbaImage>>, // index = monitor index sent to overlay-{idx}
}

fn destroy_overlays<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    for (label, window) in app.webview_windows() {
        if label.starts_with("capture-overlay-") {
            window
                .destroy()
                .map_err(|e| format!("Failed to destroy overlay {label}: {e}"))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn start_screen_capture<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    destroy_overlays(&app)?;

    let layout = app
        .available_monitors()
        .map_err(|e| format!("Failed to get monitor layout: {e}"))?;

    let captures = tokio::task::spawn_blocking(|| {
        let monitors = Monitor::all().map_err(|e| format!("Failed to get monitors: {e}"))?;
        if monitors.is_empty() {
            return Err("No monitors found".to_string());
        }
        monitors
            .iter()
            .enumerate()
            .map(|(idx, m)| {
                m.capture_image()
                    .map(|img| (img, m.is_primary()))
                    .map_err(|e| format!("Failed to capture monitor {idx}: {e}"))
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .await
    .expect("capture spawn_blocking join")?;

    if layout.len() != captures.len() {
        return Err(format!(
            "monitor count mismatch: xcap {} vs tauri {}",
            captures.len(),
            layout.len()
        ));
    }

    let primary = captures.iter().position(|(_, is_primary)| *is_primary);
    *app.state::<CaptureState>().images.lock().unwrap() =
        captures.into_iter().map(|(img, _)| img).collect();

    if let Err(e) = open_overlays(&app, &layout, primary).await {
        close_overlay_window(app)?; // teardown error wins only if teardown itself fails
        return Err(e);
    }
    Ok(())
}

async fn open_overlays<R: Runtime>(
    app: &AppHandle<R>,
    layout: &[tauri::Monitor],
    primary: Option<usize>,
) -> Result<(), String> {
    for (idx, display) in layout.iter().enumerate() {
        let scale = display.scale_factor();
        let size = display.size();
        let pos = display.position();
        let label = format!("capture-overlay-{idx}");

        let overlay = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
            .title("Screen Capture")
            .inner_size(size.width as f64 / scale, size.height as f64 / scale)
            .position(pos.x as f64 / scale, pos.y as f64 / scale)
            .transparent(true)
            .always_on_top(true)
            .decorations(false)
            .skip_taskbar(true)
            .resizable(false)
            .closable(false)
            .minimizable(false)
            .maximizable(false)
            .visible(false)
            .focused(true)
            .accept_first_mouse(true)
            .build()
            .map_err(|e| format!("Failed to create overlay window {idx}: {e}"))?;

        sleep(Duration::from_millis(100)).await; // let content load before showing

        overlay
            .show()
            .map_err(|e| format!("Failed to show overlay {idx}: {e}"))?;
        overlay // some X11 WMs ignore keep-above before map
            .set_always_on_top(true)
            .map_err(|e| format!("Failed to raise overlay {idx}: {e}"))?;
        if primary == Some(idx) {
            overlay
                .set_focus()
                .map_err(|e| format!("Failed to focus overlay {idx}: {e}"))?;
        }
    }

    if let Some(idx) = primary {
        sleep(Duration::from_millis(100)).await; // later overlays may steal focus while mapping
        app.get_webview_window(&format!("capture-overlay-{idx}"))
            .ok_or_else(|| format!("Overlay {idx} vanished before focus"))?
            .set_focus()
            .map_err(|e| format!("Failed to focus overlay {idx}: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
pub fn close_overlay_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    destroy_overlays(&app)?;
    app.state::<CaptureState>().images.lock().unwrap().clear();
    app.emit("capture-closed", ())
        .map_err(|e| format!("Failed to emit capture-closed: {e}"))
}

#[tauri::command]
pub async fn capture_selected_area<R: Runtime>(
    app: AppHandle<R>,
    coords: SelectionCoords,
    monitor_index: usize,
) -> Result<String, String> {
    let image = std::mem::take(&mut *app.state::<CaptureState>().images.lock().unwrap())
        .into_iter()
        .nth(monitor_index);
    let encoded = match image {
        None => Err(format!("No captured image for monitor {monitor_index}")),
        Some(img) => tokio::task::spawn_blocking(move || crop_to_png_base64(img, coords))
            .await
            .expect("crop spawn_blocking join"),
    };
    match encoded {
        Err(e) => {
            close_overlay_window(app)?;
            Err(e)
        }
        Ok(b64) => {
            destroy_overlays(&app)?; // no capture-closed: its TS handler would drop this selection
            app.emit("captured-selection", &b64)
                .map_err(|e| format!("Failed to emit captured-selection event: {e}"))?;
            Ok(b64)
        }
    }
}

fn crop_to_png_base64(image: RgbaImage, coords: SelectionCoords) -> Result<String, String> {
    if coords.width == 0 || coords.height == 0 {
        return Err("Invalid selection dimensions".to_string());
    }
    let (img_width, img_height) = image.dimensions();
    let x = coords.x.min(img_width.saturating_sub(1));
    let y = coords.y.min(img_height.saturating_sub(1));
    let width = coords.width.min(img_width - x);
    let height = coords.height.min(img_height - y);
    encode_png_base64(&image.view(x, y, width, height).to_image())
}

fn encode_png_base64(image: &RgbaImage) -> Result<String, String> {
    let mut png_buffer = Vec::new();
    PngEncoder::new(&mut png_buffer)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ColorType::Rgba8.into(),
        )
        .map_err(|e| format!("Failed to encode to PNG: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(png_buffer))
}

#[tauri::command]
pub async fn capture_to_base64(window: tauri::WebviewWindow) -> Result<String, String> {
    let position = window
        .outer_position()
        .map_err(|e| format!("Failed to get window position: {e}"))?;
    let size = window
        .outer_size()
        .map_err(|e| format!("Failed to get window size: {e}"))?;
    let width = size.width.min(i32::MAX as u32) as i32;
    let height = size.height.min(i32::MAX as u32) as i32;
    let window_left = position.x;
    let window_top = position.y;
    let window_right = window_left.saturating_add(width);
    let window_bottom = window_top.saturating_add(height);
    let window_center_x = window_left.saturating_add(width / 2);
    let window_center_y = window_top.saturating_add(height / 2);

    tokio::task::spawn_blocking(move || {
        let monitors = Monitor::all().map_err(|e| format!("Failed to get monitors: {e}"))?;
        if monitors.is_empty() {
            return Err("No monitors found".to_string());
        }

        let mut best_idx: Option<usize> = None;
        let mut best_area: i64 = 0;

        for (idx, monitor) in monitors.iter().enumerate() {
            let monitor_left = monitor.x();
            let monitor_top = monitor.y();
            let monitor_right = monitor_left.saturating_add(monitor.width() as i32);
            let monitor_bottom = monitor_top.saturating_add(monitor.height() as i32);

            let overlap_width =
                (window_right.min(monitor_right) - window_left.max(monitor_left)).max(0);
            let overlap_height =
                (window_bottom.min(monitor_bottom) - window_top.max(monitor_top)).max(0);
            let area = (overlap_width as i64) * (overlap_height as i64);

            if area > best_area {
                best_area = area;
                best_idx = Some(idx);
            }
        }

        let target_idx = best_idx.unwrap_or_else(|| {
            // window fully off-screen: nearest monitor centre
            let mut closest_idx = 0usize;
            let mut closest_distance = i128::MAX;
            for (idx, monitor) in monitors.iter().enumerate() {
                let monitor_center_x = monitor.x().saturating_add(monitor.width() as i32 / 2);
                let monitor_center_y = monitor.y().saturating_add(monitor.height() as i32 / 2);
                let dx = (window_center_x - monitor_center_x) as i128;
                let dy = (window_center_y - monitor_center_y) as i128;
                let distance = dx * dx + dy * dy;
                if distance < closest_distance {
                    closest_distance = distance;
                    closest_idx = idx;
                }
            }
            closest_idx
        });

        let image = monitors
            .into_iter()
            .nth(target_idx)
            .expect("index from enumerate")
            .capture_image()
            .map_err(|e| format!("Failed to capture image: {e}"))?;
        encode_png_base64(&image)
    })
    .await
    .expect("capture spawn_blocking join")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};
    use tauri::Listener;

    type Events = Arc<Mutex<Vec<&'static str>>>;

    fn app(images: &[(u32, u32)]) -> (tauri::App<MockRuntime>, Events) {
        let app = mock_builder()
            .manage(CaptureState::default())
            .build(mock_context(noop_assets()))
            .unwrap();
        *app.state::<CaptureState>().images.lock().unwrap() = images
            .iter()
            .map(|&(w, h)| RgbaImage::new(w, h))
            .collect();
        let events: Events = Arc::default();
        for name in ["capture-closed", "captured-selection"] {
            let events = events.clone();
            app.listen_any(name, move |_| events.lock().unwrap().push(name));
        }
        (app, events)
    }

    #[tokio::test]
    async fn capture_selected_area_outcomes() {
        let sel = |x, y, width, height| SelectionCoords {
            x,
            y,
            width,
            height,
        };
        #[rustfmt::skip]
        let cases: Vec<(&str, Vec<(u32, u32)>, usize, SelectionCoords, Result<(u32, u32), ()>, Vec<&str>)> = vec![
            ("index out of range", vec![(20, 20), (20, 20)], 5, sel(0, 0, 10, 10), Err(()), vec!["capture-closed"]),
            ("zero width", vec![(20, 20), (20, 20)], 0, sel(0, 0, 0, 10), Err(()), vec!["capture-closed"]),
            ("valid", vec![(20, 20)], 0, sel(5, 5, 10, 10), Ok((10, 10)), vec!["captured-selection"]),
            ("overflow clamped", vec![(20, 20)], 0, sel(15, 18, 10, 10), Ok((5, 2)), vec!["captured-selection"]),
        ];
        for (name, images, idx, coords, expected, expected_events) in cases {
            let (app, events) = app(&images);
            let got = capture_selected_area(app.handle().clone(), coords, idx)
                .await
                .map(|b64| {
                    let png = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap();
                    image::load_from_memory(&png).unwrap().dimensions()
                })
                .map_err(|_| ());
            assert_eq!(got, expected, "{name}");
            assert_eq!(*events.lock().unwrap(), expected_events, "{name}");
            assert!(
                app.state::<CaptureState>().images.lock().unwrap().is_empty(),
                "{name}: images left behind"
            );
        }
    }

    #[test]
    fn close_without_main_window_emits() {
        let (app, events) = app(&[(4, 4)]);
        close_overlay_window(app.handle().clone()).unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["capture-closed"]);
        assert!(app.state::<CaptureState>().images.lock().unwrap().is_empty());
    }
}
