//! Physical size of a monitor, read from the EDID the kernel exposes in
//! sysfs. sway's IPC has no physical size, but Mutter's DisplayConfig
//! reports one ("width-mm", "height-mm") and gnome-control-center shows the
//! diagonal computed from it.

use std::fs;
use std::path::Path;

const SYSFS_DRM: &str = "/sys/class/drm";
const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// Physical width and height in millimetres of the monitor on `connector`
/// (sway's output name, such as "DP-1"), or None when it is unknown.
pub fn physical_size_mm(connector: &str) -> Option<(i32, i32)> {
    physical_size_mm_in(Path::new(SYSFS_DRM), connector)
}

/// Same as `physical_size_mm`, with the sysfs DRM class directory given.
/// sway names DRM outputs after their connector, and sysfs names them
/// "card<N>-<connector>".
pub fn physical_size_mm_in(drm: &Path, connector: &str) -> Option<(i32, i32)> {
    let suffix = format!("-{connector}");
    let entries = fs::read_dir(drm).ok()?;
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_suffix(&suffix)
                .and_then(|card| card.strip_prefix("card"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| fs::read(entry.path().join("edid")).ok())
        .find_map(|edid| size_from_edid(&edid))
}

/// The screen size from the EDID base block (bytes 21 and 22, in cm), as
/// the kernel reports it to Mutter. None when the EDID is not valid or gives
/// no size (both bytes 0, or an aspect ratio instead of a size).
pub fn size_from_edid(edid: &[u8]) -> Option<(i32, i32)> {
    if edid.len() < 128 || edid[..8] != EDID_HEADER {
        return None;
    }
    let (width_cm, height_cm) = (edid[21], edid[22]);
    if width_cm == 0 || height_cm == 0 {
        return None;
    }
    Some((i32::from(width_cm) * 10, i32::from(height_cm) * 10))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn edid(width_cm: u8, height_cm: u8) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&EDID_HEADER);
        edid[21] = width_cm;
        edid[22] = height_cm;
        edid
    }

    #[test]
    fn size_in_mm_from_cm() {
        assert_eq!(size_from_edid(&edid(60, 34)), Some((600, 340)));
    }

    #[test]
    fn no_size_without_both_dimensions() {
        // Projectors give 0/0; EDID 1.4 puts an aspect ratio in one byte.
        assert_eq!(size_from_edid(&edid(0, 0)), None);
        assert_eq!(size_from_edid(&edid(79, 0)), None);
    }

    #[test]
    fn no_size_from_an_invalid_edid() {
        assert_eq!(size_from_edid(&[]), None);
        assert_eq!(size_from_edid(&edid(60, 34)[..127]), None);
        let mut bad = edid(60, 34);
        bad[0] = 0xff;
        assert_eq!(size_from_edid(&bad), None);
    }

    #[test]
    fn finds_the_connector_in_sysfs() {
        let drm = env::temp_dir().join(format!("displayd-edid-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&drm);
        for (dir, edid) in [
            ("card1-DP-1", edid(60, 34)),
            ("card1-DP-10", edid(70, 39)),
            ("card1-eDP-1", edid(31, 17)),
            ("card1-HDMI-A-1", Vec::new()), // nothing connected: empty edid
        ] {
            fs::create_dir_all(drm.join(dir)).unwrap();
            fs::write(drm.join(dir).join("edid"), edid).unwrap();
        }
        fs::create_dir_all(drm.join("card1")).unwrap();

        assert_eq!(physical_size_mm_in(&drm, "DP-1"), Some((600, 340)));
        assert_eq!(physical_size_mm_in(&drm, "eDP-1"), Some((310, 170)));
        assert_eq!(physical_size_mm_in(&drm, "HDMI-A-1"), None);
        assert_eq!(physical_size_mm_in(&drm, "HEADLESS-1"), None);
        assert_eq!(physical_size_mm_in(&drm.join("missing"), "DP-1"), None);
        fs::remove_dir_all(&drm).unwrap();
    }
}
