//! Builds the final mihomo config from three layers:
//!
//! 1. the subscription profile,
//! 2. the user overlay (`prepend-rules`, `append-proxies`, ... plus a deep merge
//!    of every other key),
//! 3. settings forced by conchy-vpnx (controller socket, ports, TUN/DNS).
//!
//! Merging works on raw [`serde_yaml::Value`]s so keys unknown to us pass
//! through untouched and new mihomo features need no changes here.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use serde_yaml::{Mapping, Value};

/// Rule targets that are always valid without a matching proxy or group.
pub const BUILTIN_TARGETS: &[&str] = &["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];

/// What a rule whose target no longer exists is rewritten to.
pub const FALLBACK_TARGET: &str = "DIRECT";

/// Overlay keys that are interpreted rather than deep-merged.
const NAMED_LISTS: &[(&str, &str, &str)] = &[
    ("proxies", "prepend-proxies", "append-proxies"),
    (
        "proxy-groups",
        "prepend-proxy-groups",
        "append-proxy-groups",
    ),
];
const PREPEND_RULES: &str = "prepend-rules";
const APPEND_RULES: &str = "append-rules";

/// Settings conchy-vpnx always controls, applied after the overlay.
#[derive(Debug, Clone)]
pub struct Forced {
    pub controller_socket: PathBuf,
    pub mixed_port: u16,
    pub tun: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeWarning {
    /// A rule points at a proxy or group that does not exist.
    UnknownTarget {
        rule: String,
        target: String,
        rewritten: bool,
    },
    /// A rule could not be parsed and was dropped.
    MalformedRule { rule: String },
    /// More than one `MATCH` rule; only the effective one is kept.
    ExtraMatch { rule: String },
}

impl fmt::Display for MergeWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTarget {
                rule,
                target,
                rewritten: true,
            } => write!(
                f,
                "rule `{rule}`: unknown target `{target}`, sent to {FALLBACK_TARGET}"
            ),
            Self::UnknownTarget { rule, target, .. } => write!(
                f,
                "rule `{rule}`: target `{target}` not found (may come from a proxy provider)"
            ),
            Self::MalformedRule { rule } => write!(f, "rule `{rule}` is malformed, dropped"),
            Self::ExtraMatch { rule } => write!(f, "extra MATCH rule `{rule}` dropped"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("{0} is not a YAML mapping")]
    NotAMapping(&'static str),
    #[error("`{key}` in the {layer} must be a list")]
    NotAList { layer: &'static str, key: String },
}

#[derive(Debug)]
pub struct Merged {
    pub config: Value,
    pub warnings: Vec<MergeWarning>,
}

pub fn merge(
    subscription: Value,
    overlay: Option<Value>,
    forced: &Forced,
) -> Result<Merged, MergeError> {
    let mut cfg = into_mapping(subscription, "subscription")?;
    let mut overlay = match overlay {
        None | Some(Value::Null) => Mapping::new(),
        Some(v) => into_mapping(v, "overlay")?,
    };

    for (key, pre, app) in NAMED_LISTS {
        let pre = take_list(&mut overlay, pre)?;
        let app = take_list(&mut overlay, app)?;
        let base = take_list_from(&mut cfg, key, "subscription")?;
        if base.is_empty() && pre.is_empty() && app.is_empty() {
            continue;
        }
        cfg.insert((*key).into(), Value::Sequence(merge_named(pre, base, app)));
    }

    let user_pre = take_list(&mut overlay, PREPEND_RULES)?;
    let user_app = take_list(&mut overlay, APPEND_RULES)?;
    let base_rules = take_list_from(&mut cfg, "rules", "subscription")?;

    for (k, v) in overlay {
        match cfg.get_mut(&k) {
            Some(existing) => deep_merge(existing, v),
            None => {
                cfg.insert(k, v);
            }
        }
    }

    apply_forced(&mut cfg, forced);

    let mut warnings = Vec::new();
    let rules = merge_rules(&cfg, user_pre, base_rules, user_app, &mut warnings);
    if !rules.is_empty() {
        cfg.insert("rules".into(), Value::Sequence(rules));
    }

    Ok(Merged {
        config: Value::Mapping(cfg),
        warnings,
    })
}

fn into_mapping(v: Value, what: &'static str) -> Result<Mapping, MergeError> {
    match v {
        Value::Mapping(m) => Ok(m),
        _ => Err(MergeError::NotAMapping(what)),
    }
}

fn take_list(m: &mut Mapping, key: &str) -> Result<Vec<Value>, MergeError> {
    take_list_from(m, key, "overlay")
}

fn take_list_from(
    m: &mut Mapping,
    key: &str,
    layer: &'static str,
) -> Result<Vec<Value>, MergeError> {
    match m.remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(s)) => Ok(s),
        Some(_) => Err(MergeError::NotAList {
            layer,
            key: key.into(),
        }),
    }
}

fn item_name(v: &Value) -> Option<&str> {
    v.get("name").and_then(Value::as_str)
}

/// `prepend ++ base ++ append`, where user items replace base items of the same name.
fn merge_named(pre: Vec<Value>, base: Vec<Value>, app: Vec<Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(pre.len() + base.len() + app.len());
    let user_names: HashSet<String> = pre
        .iter()
        .chain(&app)
        .filter_map(item_name)
        .map(str::to_owned)
        .collect();

    for item in pre {
        if item_name(&item).is_none_or(|n| seen.insert(n.to_owned())) {
            out.push(item);
        }
    }
    for item in base {
        if item_name(&item).is_none_or(|n| !user_names.contains(n)) {
            out.push(item);
        }
    }
    for item in app {
        if item_name(&item).is_none_or(|n| seen.insert(n.to_owned())) {
            out.push(item);
        }
    }
    out
}

/// Maps merge recursively; anything else is replaced by the overlay value.
fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Mapping(b), Value::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

fn apply_forced(cfg: &mut Mapping, forced: &Forced) {
    // A TCP controller would expose the API to every local user and web page.
    for key in [
        "external-controller",
        "external-controller-tls",
        "port",
        "socks-port",
    ] {
        cfg.remove(key);
    }
    cfg.insert(
        "external-controller-unix".into(),
        forced
            .controller_socket
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    cfg.insert("mixed-port".into(), u64::from(forced.mixed_port).into());

    let tun = section(cfg, "tun");
    tun.insert("enable".into(), forced.tun.into());
    if forced.tun {
        set_default(tun, "stack", "mixed".into());
        set_default(tun, "auto-route", true.into());
        set_default(tun, "auto-detect-interface", true.into());
        set_default(tun, "dns-hijack", Value::Sequence(vec!["any:53".into()]));

        // TUN without mihomo's DNS leaks queries and breaks domain rules.
        let dns = section(cfg, "dns");
        dns.insert("enable".into(), true.into());
        set_default(dns, "enhanced-mode", "fake-ip".into());
    }
}

/// Get `key` as a mapping, replacing it if it is missing or not a mapping.
fn section<'a>(cfg: &'a mut Mapping, key: &str) -> &'a mut Mapping {
    if !matches!(cfg.get(key), Some(Value::Mapping(_))) {
        cfg.insert(key.into(), Value::Mapping(Mapping::new()));
    }
    match cfg.get_mut(key) {
        Some(Value::Mapping(m)) => m,
        _ => unreachable!("section was just inserted"),
    }
}

fn set_default(m: &mut Mapping, key: &str, value: Value) {
    if !m.contains_key(key) {
        m.insert(key.into(), value);
    }
}

/// Split on commas that are not inside parentheses (logical rules nest them).
fn split_rule(rule: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in rule.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(rule[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(rule[start..].trim());
    parts
}

/// Index of the target field, or `None` when the rule has no checkable target.
fn target_index(kind: &str) -> Option<usize> {
    match kind {
        "MATCH" => Some(1),
        "SUB-RULE" => None,
        _ => Some(2),
    }
}

fn merge_rules(
    cfg: &Mapping,
    user_pre: Vec<Value>,
    base: Vec<Value>,
    user_app: Vec<Value>,
    warnings: &mut Vec<MergeWarning>,
) -> Vec<Value> {
    let mut known: HashSet<&str> = BUILTIN_TARGETS.iter().copied().collect();
    for key in ["proxies", "proxy-groups"] {
        if let Some(Value::Sequence(items)) = cfg.get(key) {
            known.extend(items.iter().filter_map(item_name));
        }
    }
    // Proxies from providers are only known at runtime, so don't rewrite blindly.
    let has_providers =
        matches!(cfg.get("proxy-providers"), Some(Value::Mapping(m)) if !m.is_empty());

    // Iterate user rules first so the user's MATCH is the one that wins, then
    // restore the final order: prepend, subscription, append.
    const PRE: u8 = 0;
    const BASE: u8 = 1;
    const APP: u8 = 2;
    let ordered = user_pre
        .into_iter()
        .map(|r| (r, PRE))
        .chain(user_app.into_iter().map(|r| (r, APP)))
        .chain(base.into_iter().map(|r| (r, BASE)));

    let mut rules = Vec::new();
    let mut matches = Vec::new();
    for (raw, origin) in ordered {
        let Some(text) = raw.as_str() else {
            warnings.push(MergeWarning::MalformedRule {
                rule: format!("{raw:?}"),
            });
            continue;
        };
        let mut parts = split_rule(text);
        let kind = parts[0].to_ascii_uppercase();
        let Some(idx) = target_index(&kind) else {
            rules.push((raw, origin));
            continue;
        };
        let Some(&target) = parts.get(idx) else {
            warnings.push(MergeWarning::MalformedRule { rule: text.into() });
            continue;
        };

        let mut rule = text.to_owned();
        if !known.contains(target) {
            let rewritten = !has_providers;
            warnings.push(MergeWarning::UnknownTarget {
                rule: text.into(),
                target: target.into(),
                rewritten,
            });
            if rewritten {
                parts[idx] = FALLBACK_TARGET;
                rule = parts.join(",");
            }
        }
        if kind == "MATCH" {
            matches.push(rule);
        } else {
            rules.push((Value::String(rule), origin));
        }
    }

    rules.sort_by_key(|(_, origin)| *origin);
    let mut out: Vec<Value> = rules.into_iter().map(|(r, _)| r).collect();

    // The user's MATCH (seen first) wins over the subscription's.
    let mut matches = matches.into_iter();
    if let Some(m) = matches.next() {
        out.push(Value::String(m));
    }
    warnings.extend(matches.map(|rule| MergeWarning::ExtraMatch { rule }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forced(tun: bool) -> Forced {
        Forced {
            controller_socket: "/run/user/1000/conchy-vpnx/mihomo.sock".into(),
            mixed_port: 7890,
            tun,
        }
    }

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    fn rules(v: &Value) -> Vec<&str> {
        v["rules"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect()
    }

    fn names(v: &Value, key: &str) -> Vec<String> {
        v[key]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_owned())
            .collect()
    }

    const SUB: &str = r#"
port: 7890
external-controller: 0.0.0.0:9090
proxies:
  - {name: a, type: ss}
  - {name: b, type: ss}
proxy-groups:
  - {name: Proxy, type: select, proxies: [a, b]}
rules:
  - DOMAIN-SUFFIX,google.com,Proxy
  - GEOIP,CN,DIRECT
  - MATCH,Proxy
"#;

    #[test]
    fn rule_order_is_prepend_base_append_match() {
        let overlay = yaml(
            r#"
prepend-rules: ['DOMAIN,work.corp,DIRECT']
append-rules: ['DOMAIN-KEYWORD,ads,REJECT']
"#,
        );
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        assert_eq!(
            rules(&out.config),
            [
                "DOMAIN,work.corp,DIRECT",
                "DOMAIN-SUFFIX,google.com,Proxy",
                "GEOIP,CN,DIRECT",
                "DOMAIN-KEYWORD,ads,REJECT",
                "MATCH,Proxy",
            ]
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn user_match_overrides_subscription_match() {
        let overlay = yaml("append-rules: ['MATCH,DIRECT']");
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        assert_eq!(rules(&out.config).last(), Some(&"MATCH,DIRECT"));
        assert_eq!(
            out.warnings,
            [MergeWarning::ExtraMatch {
                rule: "MATCH,Proxy".into()
            }]
        );
    }

    #[test]
    fn unknown_target_is_rewritten_to_direct() {
        let overlay = yaml("prepend-rules: ['DOMAIN,x.com,Gone,no-resolve']");
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        assert_eq!(rules(&out.config)[0], "DOMAIN,x.com,DIRECT,no-resolve");
        assert!(matches!(
            &out.warnings[0],
            MergeWarning::UnknownTarget { target, rewritten: true, .. } if target == "Gone"
        ));
    }

    #[test]
    fn unknown_target_kept_when_providers_exist() {
        let sub = format!("{SUB}\nproxy-providers: {{p: {{type: http, url: 'https://x'}}}}");
        let overlay = yaml("prepend-rules: ['DOMAIN,x.com,FromProvider']");
        let out = merge(yaml(&sub), Some(overlay), &forced(false)).unwrap();
        assert_eq!(rules(&out.config)[0], "DOMAIN,x.com,FromProvider");
        assert!(matches!(
            &out.warnings[0],
            MergeWarning::UnknownTarget {
                rewritten: false,
                ..
            }
        ));
    }

    #[test]
    fn logical_rules_target_is_checked() {
        let overlay = yaml("prepend-rules: ['AND,((DOMAIN,a.com),(NETWORK,UDP)),Proxy']");
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        assert_eq!(
            rules(&out.config)[0],
            "AND,((DOMAIN,a.com),(NETWORK,UDP)),Proxy"
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn user_proxies_and_groups_replace_by_name() {
        let overlay = yaml(
            r#"
prepend-proxies: [{name: b, type: vless}]
append-proxies: [{name: c, type: trojan}]
prepend-proxy-groups: [{name: Mine, type: select, proxies: [c]}]
"#,
        );
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        assert_eq!(names(&out.config, "proxies"), ["b", "a", "c"]);
        assert_eq!(out.config["proxies"][0]["type"], "vless");
        assert_eq!(names(&out.config, "proxy-groups"), ["Mine", "Proxy"]);
    }

    #[test]
    fn other_keys_deep_merge() {
        let sub = format!("{SUB}\ndns: {{enable: false, nameserver: [1.1.1.1]}}\nlog-level: info");
        let overlay = yaml("dns: {nameserver: [9.9.9.9], ipv6: true}\nlog-level: debug");
        let out = merge(yaml(&sub), Some(overlay), &forced(false)).unwrap();
        let dns = &out.config["dns"];
        assert_eq!(dns["enable"], false);
        assert_eq!(dns["ipv6"], true);
        assert_eq!(dns["nameserver"], yaml("[9.9.9.9]"));
        assert_eq!(out.config["log-level"], "debug");
    }

    #[test]
    fn forced_settings_win_and_remove_tcp_controller() {
        let overlay = yaml("mixed-port: 1111\nexternal-controller: 127.0.0.1:9090");
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        let c = &out.config;
        assert_eq!(c["mixed-port"], 7890);
        assert!(c.get("external-controller").is_none());
        assert!(c.get("port").is_none());
        assert_eq!(
            c["external-controller-unix"],
            "/run/user/1000/conchy-vpnx/mihomo.sock"
        );
        assert_eq!(c["tun"]["enable"], false);
    }

    #[test]
    fn tun_mode_enables_dns_but_respects_user_choices() {
        let overlay = yaml("tun: {stack: gvisor}\ndns: {enhanced-mode: redir-host}");
        let out = merge(yaml(SUB), Some(overlay), &forced(true)).unwrap();
        let c = &out.config;
        assert_eq!(c["tun"]["enable"], true);
        assert_eq!(c["tun"]["stack"], "gvisor");
        assert_eq!(c["tun"]["auto-route"], true);
        assert_eq!(c["dns"]["enable"], true);
        assert_eq!(c["dns"]["enhanced-mode"], "redir-host");
    }

    #[test]
    fn malformed_rules_are_dropped() {
        let overlay = yaml("prepend-rules: ['DOMAIN,only-two', 42]");
        let out = merge(yaml(SUB), Some(overlay), &forced(false)).unwrap();
        assert_eq!(rules(&out.config).len(), 3);
        assert_eq!(out.warnings.len(), 2);
    }

    #[test]
    fn rejects_non_mapping_input() {
        assert!(merge(yaml("[1, 2]"), None, &forced(false)).is_err());
        assert!(merge(yaml(SUB), Some(yaml("prepend-rules: oops")), &forced(false)).is_err());
    }
}
