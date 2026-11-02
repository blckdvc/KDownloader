//! The macOS dock-icon bandwidth meter.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSBezierPath, NSColor, NSCompositingOperation, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSImage, NSImageView,
    NSStringDrawing,
};
use objc2_foundation::{NSData, NSDictionary, NSPoint, NSRect, NSSize, NSString};
use std::time::Duration;

use crate::{direct, Shared};

/// Polls the session's aggregate speeds once a second and repaints the dock
/// tile on the main thread. Redraws only when the displayed values change.
pub async fn dock_loop(shared: Shared, app: tauri::AppHandle) {
    let mut last: (String, String) = (String::new(), String::new());
    loop {
        let snap = shared.session.stats_snapshot();
        let d = fmt(snap.download_speed.as_bytes() + direct::download_speed(&app));
        let u = fmt(snap.upload_speed.as_bytes());
        if (d.clone(), u.clone()) != last {
            last = (d.clone(), u.clone());
            let idle = direct::download_speed(&app) == 0 && snap.download_speed.as_bytes() == 0 && snap.upload_speed.as_bytes() == 0;
            let _ = app.run_on_main_thread(move || update(idle, &d, &u));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Compact bytes/sec formatter, e.g. "0", "850K", "1.2M".
pub fn fmt(bps: u64) -> String {
    if bps == 0 {
        return "0".into();
    }
    let b = bps as f64;
    if b < 1024.0 {
        format!("{b:.0}B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.0}K", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        let m = b / (1024.0 * 1024.0);
        if m < 10.0 {
            format!("{m:.1}M")
        } else {
            format!("{m:.0}M")
        }
    } else {
        format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn update(idle: bool, down: &str, up: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let tile = app.dockTile();

    // Idle → restore the plain app icon.
    if idle {
        tile.setContentView(None);
        tile.display();
        return;
    }

    let size = NSSize::new(128.0, 128.0);
    let image = NSImage::initWithSize(NSImage::alloc(), size);

    // The replacement draws in a callback, which is a rewrite of this function for no
    // gain: the dock tile has a fixed size.
    #[allow(deprecated)]
    image.lockFocus();
    unsafe {
        // The icon in two layers, with the mark raised so the speeds leave the sphere clear.
        let no_crop = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        let layers: [(&[u8], NSRect); 2] = [
            (
                include_bytes!("../icons/dock-tile.png"),
                NSRect::new(NSPoint::new(0.0, 0.0), size),
            ),
            (
                include_bytes!("../icons/dock-mark.png"),
                NSRect::new(NSPoint::new(13.0, 37.0), NSSize::new(102.0, 102.0)),
            ),
        ];
        for (png, rect) in layers {
            if let Some(layer) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png)) {
                layer.drawInRect_fromRect_operation_fraction(
                    rect,
                    no_crop,
                    NSCompositingOperation::SourceOver,
                    1.0,
                );
            }
        }

        // Shared font; white numbers with qBittorrent-style colored arrows.
        let font = NSFont::boldSystemFontOfSize(21.0);
        let kf: &NSString = NSFontAttributeName;
        let kc: &NSString = NSForegroundColorAttributeName;
        let vf: &AnyObject = &font;

        let white = NSColor::whiteColor();
        let green = NSColor::colorWithCalibratedRed_green_blue_alpha(0.36, 0.84, 0.44, 1.0);
        let blue = NSColor::colorWithCalibratedRed_green_blue_alpha(0.33, 0.66, 1.0, 1.0);
        let vw: &AnyObject = &white;
        let vg: &AnyObject = &green;
        let vb: &AnyObject = &blue;
        let white_attrs: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[kf, kc], &[vf, vw]);
        let green_attrs: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[kf, kc], &[vf, vg]);
        let blue_attrs: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[kf, kc], &[vf, vb]);

        let down_arrow = NSString::from_str("\u{2193}");
        let up_arrow = NSString::from_str("\u{2191}");
        let down_num = NSString::from_str(down);
        let up_num = NSString::from_str(up);

        let da = down_arrow.sizeWithAttributes(Some(&green_attrs));
        let dn = down_num.sizeWithAttributes(Some(&white_attrs));
        let ua = up_arrow.sizeWithAttributes(Some(&blue_attrs));
        let un = up_num.sizeWithAttributes(Some(&white_attrs));

        let gap = 4.0;
        let down_w = da.width + gap + dn.width;
        let up_w = ua.width + gap + un.width;
        let line_h = da.height.max(ua.height);
        let text_w = down_w.max(up_w);

        // Translucent rounded capsule hugging the text (soft corners + alpha).
        let pad_x = 8.0;
        let pad_y = 1.0;
        let line_gap = 0.0;
        let pill_w = (text_w + pad_x * 2.0).min(100.0);
        let pill_h = line_h * 2.0 + line_gap + pad_y * 2.0;
        let pill_x = (128.0 - pill_w) / 2.0;
        let pill_y = 15.0;
        let pill = NSRect::new(NSPoint::new(pill_x, pill_y), NSSize::new(pill_w, pill_h));
        let radius = pill_h / 2.0;
        let path =
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, radius, radius);
        NSColor::colorWithCalibratedWhite_alpha(0.0, 0.52).setFill();
        path.fill();

        // Download on top, upload below: colored arrow + white number, centered.
        let up_y = pill_y + pad_y;
        let down_y = up_y + line_h + line_gap;

        let dx = (128.0 - down_w) / 2.0;
        down_arrow.drawAtPoint_withAttributes(NSPoint::new(dx, down_y), Some(&green_attrs));
        down_num.drawAtPoint_withAttributes(
            NSPoint::new(dx + da.width + gap, down_y),
            Some(&white_attrs),
        );

        let ux = (128.0 - up_w) / 2.0;
        up_arrow.drawAtPoint_withAttributes(NSPoint::new(ux, up_y), Some(&blue_attrs));
        up_num.drawAtPoint_withAttributes(
            NSPoint::new(ux + ua.width + gap, up_y),
            Some(&white_attrs),
        );
    }
    #[allow(deprecated)]
    image.unlockFocus();

    let iv = NSImageView::imageViewWithImage(&image, mtm);
    tile.setContentView(Some(&iv));
    tile.display();
}
