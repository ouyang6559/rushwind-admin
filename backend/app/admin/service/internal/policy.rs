//! Pure policy matchers — the ports of `data/login_policy_checker.go`
//! helpers and the password-complexity classifier.

/// `matchIPValue`: exact IP or CIDR range (v4).
pub fn ip_matches(client_ip: &str, value: &str) -> bool {
    if client_ip.is_empty() {
        return false;
    }
    let (addr, mask) = match value.split_once('/') {
        Some((addr, mask)) => (addr, mask.parse::<u32>().unwrap_or(32)),
        None => return client_ip == value,
    };
    let (Some(client), Some(base)) = (ipv4_to_u32(client_ip), ipv4_to_u32(addr)) else {
        return false;
    };
    if mask == 0 {
        return true;
    }
    if mask > 32 {
        return false;
    }
    let prefix = u32::MAX << (32 - mask);
    (client & prefix) == (base & prefix)
}

fn ipv4_to_u32(text: &str) -> Option<u32> {
    let octets: Vec<u8> = text.split('.').filter_map(|p| p.parse().ok()).collect();
    if octets.len() != 4 {
        return None;
    }
    Some(
        ((octets[0] as u32) << 24)
            | ((octets[1] as u32) << 16)
            | ((octets[2] as u32) << 8)
            | octets[3] as u32,
    )
}

/// `matchTimeWindow`: `HH:MM-HH:MM` local-time window (end inclusive).
pub fn time_window_matches(value: &str) -> bool {
    let Some((start, end)) = value.split_once('-') else {
        return false;
    };
    let minutes = |text: &str| -> Option<u32> {
        let (h, m) = text.trim().split_once(':')?;
        Some(h.trim().parse::<u32>().ok()? * 60 + m.trim().parse::<u32>().ok()?)
    };
    let (Some(start), Some(end)) = (minutes(start), minutes(end)) else {
        return false;
    };
    let local = chrono::Local::now();
    let now = local.hour() * 60 + local.minute();
    now >= start && now <= end
}

use chrono::Timelike;

/// The password char-class count: lower/upper/digit/special — the
/// ≥3-classes complexity rule.
pub fn char_classes(password: &str) -> u32 {
    let mut classes = 0u32;
    let (mut lower, mut upper, mut digit, mut special) = (false, false, false, false);
    for ch in password.chars() {
        match ch {
            'a'..='z' => lower = true,
            'A'..='Z' => upper = true,
            '0'..='9' => digit = true,
            _ => special = true,
        }
    }
    for hit in [lower, upper, digit, special] {
        if hit {
            classes += 1;
        }
    }
    classes
}
