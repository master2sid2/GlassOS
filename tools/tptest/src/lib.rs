extern crate alloc;
#[path = "../../../src/touchpad/hid.rs"]
pub mod hid;
#[path = "../../../src/touchpad/gesture.rs"]
pub mod gesture;
#[path = "../../../src/aml.rs"]
pub mod aml;

#[cfg(test)]
mod aml_tests {
    use super::aml::*;
    /// The laptop DSDT (Dell Latitude 7410, from acpidump): the path is given in the DSDT
    /// environment variable; without it the DSDT tests are skipped.
    fn dsdt() -> Option<Vec<u8>> {
        std::fs::read(std::env::var("DSDT").ok()?).ok()
    }

    #[test]
    fn laptop_dsdt() {
        let Some(d) = dsdt() else { return eprintln!("no DSDT dump, skipped") };
        assert_eq!(memory_region(&d, b"GNVS"), Some((0x4eff1000, 0x7fe)));
        assert_eq!(memory_region(&d, b"PNVA"), Some((0x4f03bb98, 0x22a)));
        assert_eq!(field(&d, b"GNVS", b"GPDI"), Some((0x47a, 32)));
        assert_eq!(field(&d, b"GNVS", b"TPDM"), Some((0x697, 8)));
        assert_eq!(field(&d, b"PNVA", b"SBRG"), Some((0x7a, 32)));
        assert_eq!(field(&d, b"PNVA", b"PCHS"), Some((0, 16)));
        assert_eq!(name_int(&d, b"PCHL"), Some(2));
        let lp = packages(&d, b"GPCL").expect("GPCL");
        assert_eq!(lp[0], vec![0x6e0000, 0x19, 0x600, 0xb0, 0x20, 0x140, 0]);
        let h = packages(&d, b"GPCH").expect("GPCH");
        println!("GPCL {} groups, GPCH {} groups", lp.len(), h.len());
        for (i, g) in lp.iter().enumerate() { println!("{i}: {:x?}", g); }
    }
}

#[cfg(test)]
mod tests {
    use super::gesture::*;
    use super::hid::*;

    fn layout() -> (Vec<Field>, Layout) {
        let desc = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/touchpad.bin")).unwrap();
        let fields = parse(&desc);
        let l = touchpad(&fields).expect("touchpad");
        (fields, l)
    }

    #[test]
    fn layout_of_the_laptop() {
        let (fields, l) = layout();
        assert_eq!(l.report, 8);
        assert_eq!(l.fingers.len(), 1);
        let f = l.fingers[0];
        assert_eq!((f.x.logical_max, f.y.logical_max), (1343, 767));
        assert!((l.width_mm - 112.0).abs() < 0.5, "{}", l.width_mm);
        assert!((l.height_mm - 64.0).abs() < 0.5, "{}", l.height_mm);
        assert!(l.count.is_some() && l.button.is_some() && l.scan_time.is_some() && f.id.is_some() && f.confidence.is_some());
        let mode = l.input_mode.unwrap();
        assert_eq!((mode.report, mode.offset, mode.size), (11, 0, 8));
        let (s, b) = (l.surface_switch.unwrap(), l.button_switch.unwrap());
        assert_eq!((s.report, s.offset, b.report, b.offset), (12, 0, 12, 1));
        assert_eq!(report_bytes(&fields, Kind::Input, 8), 9);
    }

    #[test]
    fn reads_a_real_report_from_the_laptop() {
        // touchpad: report 0c0008030b0380018f930100 (dmesg from the laptop)
        let (_, l) = layout();
        let r = [0x0c, 0x00, 0x08, 0x03, 0x0b, 0x03, 0x80, 0x01, 0x8f, 0x93, 0x01, 0x00];
        let d = &r[3..];
        let f = l.fingers[0];
        assert_eq!((f.confidence.unwrap().get(d), f.tip.get(d), f.id.unwrap().get(d)), (1, 1, 0));
        assert_eq!((f.x.get(d), f.y.get(d)), (0x030b, 0x0180));
        assert_eq!(l.scan_time.unwrap().get(d), 0x938f);
        assert_eq!((l.count.unwrap().get(d), l.button.unwrap().get(d)), (1, 0));
    }

    fn frame(t: f32, contacts: &[(u32, f32, f32)], button: bool) -> Frame {
        Frame { contacts: contacts.iter().map(|&(id, x, y)| Contact { id, x, y }).collect(), button, time: t }
    }

    #[test]
    fn one_finger_moves_with_acceleration() {
        let mut g = Gestures::new(112.0, 64.0);
        let mut slow = 0;
        for i in 0..10 {
            for s in g.frame(&frame(i as f32 * 10.0, &[(1, 20.0 + i as f32 * 0.1, 30.0)], false)) { slow += s.dx; }
        }
        g.frame(&frame(200.0, &[], false));
        let mut fast = 0;
        for i in 0..10 {
            for s in g.frame(&frame(300.0 + i as f32 * 10.0, &[(2, 20.0 + i as f32 * 3.0, 30.0)], false)) { fast += s.dx; }
        }
        assert!(slow >= 4 && slow <= 6, "{slow}");
        assert!(fast > 27 * 12, "{fast}");
    }

    #[test]
    fn tap_clicks() {
        let mut g = Gestures::new(112.0, 64.0);
        assert!(g.frame(&frame(0.0, &[(1, 50.0, 30.0)], false)).is_empty());
        g.frame(&frame(50.0, &[(1, 50.3, 30.1)], false));
        let out = g.frame(&frame(100.0, &[], false));
        assert_eq!(out.iter().map(|s| s.buttons).collect::<Vec<_>>(), vec![LEFT, 0]);
    }

    #[test]
    fn two_finger_tap_is_right_click() {
        let mut g = Gestures::new(112.0, 64.0);
        g.frame(&frame(0.0, &[(1, 40.0, 30.0)], false));
        g.frame(&frame(20.0, &[(1, 40.0, 30.0), (2, 60.0, 30.0)], false));
        g.frame(&frame(80.0, &[(2, 60.0, 30.0)], false));
        let out = g.frame(&frame(100.0, &[], false));
        assert_eq!(out.iter().map(|s| s.buttons).collect::<Vec<_>>(), vec![RIGHT, 0]);
    }

    #[test]
    fn two_fingers_scroll_with_content() {
        let mut g = Gestures::new(112.0, 64.0);
        let (mut wheel, mut moved) = (0, 0);
        for i in 0..20 {
            let y = 20.0 + i as f32;
            for s in g.frame(&frame(i as f32 * 10.0, &[(1, 40.0, y), (2, 60.0, y)], false)) { wheel += s.wheel; moved += s.dx.abs() + s.dy.abs(); }
        }
        let out = g.frame(&frame(500.0, &[], false));
        assert!(wheel <= -6 && wheel >= -8, "{wheel}");
        assert_eq!(moved, 0);
        assert!(out.iter().all(|s| s.buttons == 0));
    }

    #[test]
    fn press_in_corner_is_right_button() {
        let mut g = Gestures::new(112.0, 64.0);
        g.frame(&frame(0.0, &[(1, 100.0, 58.0)], false));
        assert_eq!(g.frame(&frame(30.0, &[(1, 100.0, 58.0)], true)).last().unwrap().buttons, RIGHT);
        assert_eq!(g.frame(&frame(120.0, &[(1, 100.0, 58.0)], false)).last().unwrap().buttons, 0);
        assert!(g.frame(&frame(150.0, &[], false)).iter().all(|s| s.buttons == 0));
    }

    #[test]
    fn mouse_report_of_the_laptop() {
        let (fields, _) = layout();
        let m = mouse(&fields).expect("mouse");
        assert_eq!(m.report, 1);
        assert_eq!(m.buttons.len(), 3);
        assert!(m.wheel.is_some());
        // The right button, motion -3 in X and +5 in Y, wheel -1.
        let data = [0b010, 0xFD, 0x05, 0xFF, 0x00];
        assert_eq!(m.read(&data), (2, -3, 5, -1));
    }
}
