//! STS `AssumeRoleWithWebIdentity` (EKS IRSA and any OIDC federation) and the
//! minimal XML / timestamp parsing it needs.

use super::AuthError;
use super::sigv4::{AwsCredentials, uri_encode};

/// Text of the first `<tag>…</tag>` element, entity-unescaped.
///
/// A deliberately small reader for the flat, well-known STS response shape:
/// no attributes on the element itself, no CDATA, no nesting of the same tag.
pub fn xml_text(document: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = document.find(&open)? + open.len();
    let end = start + document.get(start..)?.find(&close)?;
    Some(unescape(document.get(start..end)?.trim()))
}

fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#xD;", "\r")
        .replace("&#xA;", "\n")
        .replace("&amp;", "&")
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` into Unix seconds.
pub fn parse_iso8601(value: &str) -> Option<u64> {
    let value = value.trim();
    let number = |range: std::ops::Range<usize>| -> Option<i64> { value.get(range)?.parse().ok() };
    if value.get(4..5)? != "-" || value.get(10..11).map(str::to_ascii_uppercase)? != "T" {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = value.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        rest = fraction.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset_secs = match rest {
        "Z" | "z" | "" => 0,
        zone => {
            let sign = match zone.get(..1)? {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(4..6).unwrap_or("00").parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let total = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    u64::try_from(total).ok()
}

/// Inputs of one `AssumeRoleWithWebIdentity` call.
#[derive(Debug)]
pub(crate) struct WebIdentityRequest<'a> {
    /// STS endpoint base URL (operator config; default `https://sts.<region>.amazonaws.com`).
    pub endpoint: &'a str,
    pub role_arn: &'a str,
    pub session_name: &'a str,
    pub token: &'a str,
}

/// Credentials and their expiry (Unix seconds).
pub(crate) type Expiring = (AwsCredentials, Option<u64>);

/// Form body of the (unsigned) STS call.
pub(crate) fn web_identity_form(request: &WebIdentityRequest<'_>) -> String {
    let pairs = [
        ("Action", "AssumeRoleWithWebIdentity"),
        ("Version", "2011-06-15"),
        ("RoleArn", request.role_arn),
        ("RoleSessionName", request.session_name),
        ("WebIdentityToken", request.token),
    ];
    pairs.iter().map(|(k, v)| format!("{k}={}", uri_encode(v, false))).collect::<Vec<_>>().join("&")
}

/// Parse a successful or error STS XML response.
pub(crate) fn parse_web_identity_response(status: u16, body: &str) -> Result<Expiring, AuthError> {
    if !(200..300).contains(&status) {
        let code = xml_text(body, "Code").unwrap_or_else(|| format!("HTTP {status}"));
        let message = xml_text(body, "Message").unwrap_or_default();
        return Err(AuthError::new("web-identity", format!("STS {code}: {message}")));
    }
    let credentials = xml_text(body, "Credentials")
        .ok_or_else(|| AuthError::new("web-identity", "STS response has no <Credentials>"))?;
    let field = |tag: &str| {
        xml_text(&credentials, tag)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AuthError::new("web-identity", format!("STS response has no <{tag}>")))
    };
    let creds = AwsCredentials {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: Some(field("SessionToken")?),
    };
    let expires = xml_text(&credentials, "Expiration").and_then(|e| parse_iso8601(&e));
    Ok((creds, expires))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <SubjectFromWebIdentityToken>system:serviceaccount:gw:graphrt</SubjectFromWebIdentityToken>
    <Credentials>
      <SessionToken>FQoGZXIvYXdzE&amp;Token</SessionToken>
      <SecretAccessKey>wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY</SecretAccessKey>
      <Expiration>2026-11-22T12:00:00Z</Expiration>
      <AccessKeyId>ASgeIAIOSFODNN7EXAMPLE</AccessKeyId>
    </Credentials>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>"#;

    #[test]
    fn parses_sts_success() {
        let (creds, exp) = parse_web_identity_response(200, OK).unwrap();
        assert_eq!(creds.access_key_id, "ASgeIAIOSFODNN7EXAMPLE");
        assert_eq!(creds.session_token.as_deref(), Some("FQoGZXIvYXdzE&Token"));
        assert_eq!(exp, parse_iso8601("2026-11-22T12:00:00Z"));
    }

    #[test]
    fn parses_sts_error_without_leaking_token() {
        let body = "<ErrorResponse><Error><Type>Sender</Type><Code>InvalidIdentityToken</Code>\
                    <Message>Token expired</Message></Error></ErrorResponse>";
        let err = parse_web_identity_response(400, body).unwrap_err();
        assert_eq!(err.message, "STS InvalidIdentityToken: Token expired");
        assert!(parse_web_identity_response(200, "<x/>").is_err());
    }

    #[test]
    fn iso8601_variants() {
        assert_eq!(parse_iso8601("2015-08-30T12:36:00Z"), Some(1_440_938_160));
        assert_eq!(parse_iso8601("2015-08-30T12:36:00.123Z"), Some(1_440_938_160));
        assert_eq!(parse_iso8601("2015-08-30T14:36:00+02:00"), Some(1_440_938_160));
        assert_eq!(parse_iso8601("2024-02-29T23:59:59Z"), Some(1_709_251_199));
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2015-13-30T12:36:00Z"), None);
        assert_eq!(parse_iso8601("not a date"), None);
    }

    #[test]
    fn form_encodes_token() {
        let form = web_identity_form(&WebIdentityRequest {
            endpoint: "https://sts.example",
            role_arn: "arn:aws:iam::1:role/r",
            session_name: "graphrt",
            token: "a.b+c/=",
        });
        assert!(form.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr"), "{form}");
        assert!(form.ends_with("WebIdentityToken=a.b%2Bc%2F%3D"), "{form}");
    }
}
