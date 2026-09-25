use std::collections::BTreeSet;

pub const STATE_PATH: &str = "/data/local/tmp/emutrim_state.v1";
pub const BOOT_CRITICAL: &[&str] = &["com.google.android.bluetooth"];

pub const SETTINGS: &[(&str, &str, &str, &str)] = &[
    ("animations", "global", "window_animation_scale", "0"),
    ("animations", "global", "transition_animation_scale", "0"),
    ("animations", "global", "animator_duration_scale", "0"),
    ("bglimit", "global", "background_process_limit", "4"),
    (
        "bglimit",
        "global",
        "activity_manager_constants",
        "max_cached_processes=4",
    ),
    ("sync", "global", "auto_sync", "0"),
    ("location", "secure", "location_mode", "0"),
    ("setup", "secure", "user_setup_complete", "1"),
    ("setup", "global", "device_provisioned", "1"),
    ("bluetooth", "global", "bluetooth_on", "0"),
];

// These are upstream's standard profile, excluding Android 16+ boot-critical Bluetooth.
pub const STANDARD: &[&str] = &[
    "com.google.android.googlequicksearchbox",
    "com.google.android.as",
    "com.google.android.as.oss",
    "com.android.bluetoothmidiservice",
    "com.google.android.adservices.api",
    "com.google.mainline.adservices",
    "com.google.android.apps.wellbeing",
    "com.google.android.settings.intelligence",
    "com.android.emulator.multidisplay",
    "com.google.android.apps.bard",
    "com.google.android.apps.restore",
    "com.google.android.apps.pulse",
    "com.google.android.configupdater",
    "com.google.android.projection.gearhead",
    "com.google.android.healthconnect.controller",
    "com.android.imsserviceentitlement",
    "com.android.carrierdefaultapp",
    "com.google.android.ondevicepersonalization.services",
    "com.google.android.federatedcompute",
    "com.google.android.glasses.companion",
    "com.google.android.glasses.core",
    "com.google.android.apps.safetyhub",
    "com.google.android.markup",
    "com.google.android.apps.photos",
    "com.google.android.youtube",
    "com.google.android.videos",
    "com.google.android.music",
    "com.google.android.apps.youtube.music",
    "com.google.android.apps.maps",
    "com.google.android.gm",
    "com.google.android.apps.docs",
    "com.android.gallery3d",
    "com.android.music",
    "com.android.camera2",
    "com.android.cameraextensions",
    "com.android.DeviceAsWebcam",
    "com.google.android.apps.messaging",
    "com.google.android.dialer",
    "com.google.android.contacts",
    "com.android.mms",
    "com.android.dialer",
    "com.android.contacts",
    "com.google.android.cellbroadcastservice",
    "com.android.cellbroadcastreceiver",
    "com.android.telephony.satellite",
    "com.android.stk",
    "com.google.android.tts",
    "com.google.android.marvin.talkback",
    "com.google.android.apps.accessibility.voiceaccess",
    "com.android.printspooler",
    "com.android.bips",
    "com.google.android.printservice.recommendation",
    "com.google.android.feedback",
    "com.android.traceur",
    "com.android.dreams.basic",
    "com.android.dreams.phototable",
    "com.android.wallpaper.livepicker",
    "com.google.android.apps.wallpaper",
    "com.android.emergency",
    "com.android.calendar",
    "com.google.android.calendar",
    "com.android.deskclock",
    "com.google.android.deskclock",
    "com.android.calculator2",
    "com.google.android.calculator",
];

pub fn installed_packages(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn targets(
    installed: &BTreeSet<String>,
    keep: &BTreeSet<String>,
    skip: &BTreeSet<String>,
) -> Vec<String> {
    STANDARD
        .iter()
        .filter(|pkg| {
            installed.contains(**pkg)
                && !keep.contains(**pkg)
                && !BOOT_CRITICAL.contains(pkg)
                && !(skip.contains("bluetooth") && **pkg == "com.android.bluetoothmidiservice")
        })
        .map(|p| (*p).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packages_filter_and_deduplicate() {
        let p = installed_packages(
            "package:com.google.android.apps.maps\npackage:com.google.android.apps.maps\nnoise\n",
        );
        assert_eq!(p.len(), 1);
        assert_eq!(
            targets(&p, &BTreeSet::new(), &BTreeSet::new()),
            vec!["com.google.android.apps.maps"]
        );
    }
    #[test]
    fn keep_and_skip_apply() {
        let p = installed_packages(
            "package:com.google.android.apps.maps\npackage:com.android.bluetoothmidiservice\n",
        );
        let keep = ["com.google.android.apps.maps".into()]
            .into_iter()
            .collect();
        let skip = ["bluetooth".into()].into_iter().collect();
        assert!(targets(&p, &keep, &skip).is_empty());
    }
}
