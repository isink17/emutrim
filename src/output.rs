use serde::Serialize;
use std::cell::RefCell;

#[derive(Serialize)]
pub struct DoctorCheck {
    name: String,
    status: &'static str,
    message: String,
    details: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize)]
pub struct DoctorReport {
    checks: Vec<DoctorCheck>,
    warnings: usize,
    failures: usize,
}

thread_local! { static DOCTOR_CHECKS: RefCell<Option<Vec<DoctorCheck>>> = const { RefCell::new(None) }; }

pub fn begin_doctor_json(enabled: bool) {
    DOCTOR_CHECKS.with(|checks| *checks.borrow_mut() = enabled.then(Vec::new));
}

pub fn report_doctor_check(status: &str, message: &str) -> bool {
    DOCTOR_CHECKS.with(|checks| {
        let mut checks = checks.borrow_mut();
        let Some(checks) = checks.as_mut() else {
            return false;
        };
        let name = message
            .split_once(':')
            .map(|(name, _)| name)
            .unwrap_or_else(|| message.split_whitespace().next().unwrap_or("check"))
            .trim()
            .to_ascii_lowercase()
            .replace(' ', "_");
        checks.push(DoctorCheck {
            name,
            status: match status {
                "FAIL" => "error",
                "WARN" => "warning",
                _ => "ok",
            },
            message: message.to_owned(),
            details: std::collections::BTreeMap::new(),
        });
        true
    })
}

pub fn finish_doctor_json(failures: usize) -> DoctorReport {
    let checks = DOCTOR_CHECKS.with(|checks| checks.borrow_mut().take().unwrap_or_default());
    DoctorReport {
        warnings: checks
            .iter()
            .filter(|check| check.status == "warning")
            .count(),
        failures,
        checks,
    }
}

#[derive(Serialize)]
struct Envelope<'a, T: Serialize> {
    schema_version: u8,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PublicError<'a>>,
}

#[derive(Serialize)]
struct PublicError<'a> {
    code: &'a str,
    message: &'a str,
}

pub fn success<T: Serialize>(data: T) {
    println!(
        "{}",
        serde_json::to_string(&Envelope {
            schema_version: 1,
            ok: true,
            data: Some(data),
            error: None,
        })
        .expect("serializing JSON envelope cannot fail")
    );
}

pub fn failure(code: &str, message: &str) {
    println!(
        "{}",
        serde_json::to_string(&Envelope::<()> {
            schema_version: 1,
            ok: false,
            data: None,
            error: Some(PublicError { code, message }),
        })
        .expect("serializing JSON envelope cannot fail")
    );
}

pub fn failure_with_data<T: Serialize>(code: &str, message: &str, data: T) {
    println!(
        "{}",
        serde_json::to_string(&Envelope {
            schema_version: 1,
            ok: false,
            data: Some(data),
            error: Some(PublicError { code, message }),
        })
        .expect("serializing JSON envelope cannot fail")
    );
}

pub fn code_for(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::InvalidInput => "invalid_arguments",
        std::io::ErrorKind::NotFound => "target_not_found",
        std::io::ErrorKind::PermissionDenied => "target_unauthorized",
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
            "adb_unavailable"
        }
        _ => "internal_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_envelope_keeps_numeric_version_and_unicode_error_text() {
        let text = serde_json::to_string(&Envelope {
            schema_version: 1,
            ok: false,
            data: Option::<()>::None,
            error: Some(PublicError {
                code: "invalid_arguments",
                message: "AVD živi 不存在",
            }),
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["message"], "AVD živi 不存在");
    }
}
