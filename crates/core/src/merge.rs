//! Builds the final mihomo config from layers:
//!
//! 1. the subscription profile,
//! 2. overlays, each applied on top of everything before it: remote ones
//!    first, the user's `overlay.yaml` last,
//! 3. settings forced by conchy-vpnx (controller socket, ports, TUN/DNS).
//!
//! An overlay can delete or retarget earlier rules, delete or edit proxies and
//! groups, add its own (`prepend-rules`, `append-proxies`, ...) and deep-merge
//! any other key. Merging works on raw [`serde_yaml::Value`]s so keys unknown
//! to us pass through untouched and new mihomo features need no changes here.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

use regex::Regex;
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
const DELETE_RULES: &str = "delete-rules";
const REPLACE_TARGETS: &str = "replace-rule-targets";
const DELETE_PROXIES: &str = "delete-proxies";
const DELETE_GROUPS: &str = "delete-proxy-groups";
const MODIFY_GROUPS: &str = "modify-proxy-groups";

/// Group keys that fill a group without a `proxies` list.
const GROUP_SOURCES: &[&str] = &[
    "use",
    "include-all",
    "include-all-proxies",
    "include-all-providers",
];

/// Settings conchy-vpnx always controls, applied after the overlays.
#[derive(Debug, Clone)]
pub struct Forced {
    pub controller_socket: PathBuf,
    pub mixed_port: u16,
    pub tun: bool,
    /// Let other machines use the proxy. Off unless the user opts in, whatever
    /// the subscription or an overlay says.
    pub allow_lan: bool,
}

/// One overlay and where it came from, for messages.
#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub value: Value,
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
    /// A delete/replace entry matched nothing, e.g. the subscription changed.
    Unused {
        layer: String,
        key: &'static str,
        entry: String,
    },
    /// `modify-proxy-groups` names a group that does not exist.
    UnknownGroup { layer: String, group: String },
    /// Every proxy of a group was removed; DIRECT keeps the config valid.
    EmptyGroup { group: String },
    /// A listener open to the network was dropped (`allow-lan` is off).
    ExposedListener { name: String, listen: String },
    /// mihomo's DNS server was moved to loopback (`allow-lan` is off).
    ExposedDns { listen: String },
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
            Self::Unused { layer, key, entry } => {
                write!(f, "{layer}: `{key}` entry `{entry}` matched nothing")
            }
            Self::UnknownGroup { layer, group } => {
                write!(f, "{layer}: `{MODIFY_GROUPS}`: no group named `{group}`")
            }
            Self::EmptyGroup { group } => {
                write!(
                    f,
                    "group `{group}` has no proxies left, {FALLBACK_TARGET} added"
                )
            }
            Self::ExposedListener { name, listen } => write!(
                f,
                "listener `{name}` on {listen} dropped: it would be reachable from the network \
                 (set `allow-lan true` in vpnx.kdl to allow)"
            ),
            Self::ExposedDns { listen } => write!(
                f,
                "DNS server moved from {listen} to 127.0.0.1 \
                 (set `allow-lan true` in vpnx.kdl to allow)"
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("{0} is not a YAML mapping")]
    NotAMapping(String),
    #[error("`{key}` in {layer} must be a {expected}")]
    WrongType {
        layer: String,
        key: String,
        expected: &'static str,
    },
    #[error("{layer}: bad pattern `{pattern}` in `{key}`: {error}")]
    BadPattern {
        layer: String,
        key: &'static str,
        pattern: String,
        error: regex::Error,
    },
}

#[derive(Debug)]
pub struct Merged {
    pub config: Value,
    pub warnings: Vec<MergeWarning>,
}

/// A rule and the layer it came from (0 = subscription).
type Rule = (Value, usize);

pub fn merge(
    subscription: Value,
    layers: Vec<Layer>,
    forced: &Forced,
) -> Result<Merged, MergeError> {
    let mut cfg = into_mapping(subscription, "the subscription")?;
    let mut rules: Vec<Rule> = take_list_from(&mut cfg, "rules", "the subscription")?
        .into_iter()
        .map(|r| (r, 0))
        .collect();
    let mut warnings = Vec::new();

    for (i, layer) in layers.into_iter().enumerate() {
        let name = layer.name;
        let overlay = match layer.value {
            Value::Null => Mapping::new(),
            v => into_mapping(v, &name)?,
        };
        apply_layer(&mut cfg, &mut rules, overlay, &name, i + 1, &mut warnings)?;
    }

    apply_forced(&mut cfg, forced, &mut warnings);

    let rules = finish_rules(&cfg, rules, &mut warnings);
    if !rules.is_empty() {
        cfg.insert("rules".into(), Value::Sequence(rules));
    }

    Ok(Merged {
        config: Value::Mapping(cfg),
        warnings,
    })
}

fn apply_layer(
    cfg: &mut Mapping,
    rules: &mut Vec<Rule>,
    mut overlay: Mapping,
    name: &str,
    index: usize,
    warnings: &mut Vec<MergeWarning>,
) -> Result<(), MergeError> {
    // Removals and edits apply to what earlier layers built...
    for pattern in patterns(&mut overlay, DELETE_RULES, name)? {
        let before = rules.len();
        rules.retain(|(r, _)| !r.as_str().is_some_and(|r| pattern.matches_rule(r)));
        if rules.len() == before {
            warnings.push(pattern.unused(name, DELETE_RULES));
        }
    }
    for (from, to) in string_map(&mut overlay, REPLACE_TARGETS, name)? {
        let mut used = false;
        for (rule, _) in rules.iter_mut() {
            if let Some(new) = rule.as_str().and_then(|text| retarget(text, &from, &to)) {
                *rule = Value::String(new);
                used = true;
            }
        }
        if !used {
            warnings.push(MergeWarning::Unused {
                layer: name.into(),
                key: REPLACE_TARGETS,
                entry: from,
            });
        }
    }
    let mut deleted = Vec::new();
    for (key, list) in [(DELETE_PROXIES, "proxies"), (DELETE_GROUPS, "proxy-groups")] {
        for pattern in patterns(&mut overlay, key, name)? {
            let Some(Value::Sequence(items)) = cfg.get_mut(list) else {
                warnings.push(pattern.unused(name, key));
                continue;
            };
            let before = items.len();
            items.retain(|item| match item_name(item) {
                Some(n) if pattern.matches_name(n) => {
                    deleted.push(n.to_owned());
                    false
                }
                _ => true,
            });
            if items.len() == before {
                warnings.push(pattern.unused(name, key));
            }
        }
    }
    if !deleted.is_empty() {
        remove_group_members(cfg, &deleted, warnings);
    }

    // ...then this layer adds its own.
    for (key, pre, app) in NAMED_LISTS {
        let pre = take_list_from(&mut overlay, pre, name)?;
        let app = take_list_from(&mut overlay, app, name)?;
        if pre.is_empty() && app.is_empty() {
            continue;
        }
        let base = take_list_from(cfg, key, name)?;
        cfg.insert((*key).into(), Value::Sequence(merge_named(pre, base, app)));
    }
    modify_groups(cfg, &mut overlay, name, warnings)?;

    let pre = take_list_from(&mut overlay, PREPEND_RULES, name)?;
    let app = take_list_from(&mut overlay, APPEND_RULES, name)?;
    if !pre.is_empty() || !app.is_empty() {
        let earlier = std::mem::take(rules);
        rules.extend(pre.into_iter().map(|r| (r, index)));
        rules.extend(earlier);
        rules.extend(app.into_iter().map(|r| (r, index)));
    }

    for (k, v) in overlay {
        match cfg.get_mut(&k) {
            Some(existing) => deep_merge(existing, v),
            None => {
                cfg.insert(k, v);
            }
        }
    }
    Ok(())
}

/// A delete entry: rule fields (`DOMAIN-SUFFIX,example.com` matches that rule
/// whatever its target), an exact name, or `/regex/`.
enum Pattern {
    Regex(Regex),
    Literal(String),
}

impl Pattern {
    fn parse(raw: &str, name: &str, key: &'static str) -> Result<Self, MergeError> {
        match raw.strip_prefix('/').and_then(|r| r.strip_suffix('/')) {
            Some(re) if !re.is_empty() => {
                Regex::new(re)
                    .map(Self::Regex)
                    .map_err(|error| MergeError::BadPattern {
                        layer: name.into(),
                        key,
                        pattern: raw.into(),
                        error,
                    })
            }
            _ => Ok(Self::Literal(raw.trim().to_owned())),
        }
    }

    fn matches_rule(&self, rule: &str) -> bool {
        match self {
            Self::Regex(re) => re.is_match(rule),
            Self::Literal(prefix) => {
                let want = split_rule(prefix);
                let have = split_rule(rule);
                want.len() <= have.len()
                    && want.iter().zip(&have).enumerate().all(|(i, (w, h))| {
                        // Rule types are case-insensitive, payloads are not.
                        if i == 0 {
                            w.eq_ignore_ascii_case(h)
                        } else {
                            w == h
                        }
                    })
            }
        }
    }

    fn matches_name(&self, name: &str) -> bool {
        match self {
            Self::Regex(re) => re.is_match(name),
            Self::Literal(literal) => literal == name,
        }
    }

    fn unused(&self, layer: &str, key: &'static str) -> MergeWarning {
        MergeWarning::Unused {
            layer: layer.into(),
            key,
            entry: match self {
                Self::Regex(re) => format!("/{}/", re.as_str()),
                Self::Literal(l) => l.clone(),
            },
        }
    }
}

fn patterns(m: &mut Mapping, key: &'static str, layer: &str) -> Result<Vec<Pattern>, MergeError> {
    take_list_from(m, key, layer)?
        .iter()
        .map(|v| match v.as_str() {
            Some(s) => Pattern::parse(s, layer, key),
            None => Err(wrong_type(layer, key, "list of strings")),
        })
        .collect()
}

fn string_map(
    m: &mut Mapping,
    key: &'static str,
    layer: &str,
) -> Result<Vec<(String, String)>, MergeError> {
    match m.remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Mapping(map)) => map
            .into_iter()
            .map(|(k, v)| match (k.as_str(), v.as_str()) {
                (Some(k), Some(v)) => Ok((k.to_owned(), v.to_owned())),
                _ => Err(wrong_type(layer, key, "mapping of names to names")),
            })
            .collect(),
        Some(_) => Err(wrong_type(layer, key, "mapping")),
    }
}

fn wrong_type(layer: &str, key: &str, expected: &'static str) -> MergeError {
    MergeError::WrongType {
        layer: layer.into(),
        key: key.into(),
        expected,
    }
}

/// The rule with target `from` replaced by `to`, if it targets `from`.
fn retarget(rule: &str, from: &str, to: &str) -> Option<String> {
    let mut parts = split_rule(rule);
    let idx = target_index(&parts[0].to_ascii_uppercase())?;
    if parts.get(idx) != Some(&from) {
        return None;
    }
    parts[idx] = to;
    Some(parts.join(","))
}

fn group_proxies(group: &mut Value) -> Option<&mut Vec<Value>> {
    match group.get_mut("proxies") {
        Some(Value::Sequence(s)) => Some(s),
        _ => None,
    }
}

/// Keep a group valid after its proxies were removed.
fn ensure_not_empty(group: &mut Value, warnings: &mut Vec<MergeWarning>) {
    let filled_elsewhere = GROUP_SOURCES.iter().any(|k| group.get(k).is_some());
    let name = item_name(group).unwrap_or("?").to_owned();
    if filled_elsewhere {
        return;
    }
    if let Some(list) = group_proxies(group) {
        if list.is_empty() {
            list.push(FALLBACK_TARGET.into());
            warnings.push(MergeWarning::EmptyGroup { group: name });
        }
    }
}

/// Drop deleted proxies and groups from every group that lists them.
fn remove_group_members(cfg: &mut Mapping, deleted: &[String], warnings: &mut Vec<MergeWarning>) {
    let Some(Value::Sequence(groups)) = cfg.get_mut("proxy-groups") else {
        return;
    };
    for group in groups {
        let Some(list) = group_proxies(group) else {
            continue;
        };
        let before = list.len();
        list.retain(|p| !p.as_str().is_some_and(|p| deleted.iter().any(|d| d == p)));
        if list.len() != before {
            ensure_not_empty(group, warnings);
        }
    }
}

/// `modify-proxy-groups: {Name: {prepend-proxies, append-proxies,
/// remove-proxies, <any other group key>}}`.
fn modify_groups(
    cfg: &mut Mapping,
    overlay: &mut Mapping,
    layer: &str,
    warnings: &mut Vec<MergeWarning>,
) -> Result<(), MergeError> {
    let edits = match overlay.remove(MODIFY_GROUPS) {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Mapping(m)) => m,
        Some(_) => return Err(wrong_type(layer, MODIFY_GROUPS, "mapping of group names")),
    };
    for (group_name, edit) in edits {
        let Some(group_name) = group_name.as_str().map(str::to_owned) else {
            return Err(wrong_type(layer, MODIFY_GROUPS, "mapping of group names"));
        };
        let mut edit = match edit {
            Value::Mapping(m) => m,
            _ => return Err(wrong_type(layer, MODIFY_GROUPS, "mapping per group")),
        };
        let key = |k: &str| format!("{MODIFY_GROUPS}.{group_name}.{k}");
        let remove: Vec<Pattern> = take_list_from(&mut edit, "remove-proxies", layer)?
            .iter()
            .map(|v| match v.as_str() {
                Some(s) => Pattern::parse(s, layer, MODIFY_GROUPS),
                None => Err(wrong_type(layer, &key("remove-proxies"), "list of strings")),
            })
            .collect::<Result<_, _>>()?;
        let pre = take_list_from(&mut edit, "prepend-proxies", layer)?;
        let app = take_list_from(&mut edit, "append-proxies", layer)?;

        let group = match cfg.get_mut("proxy-groups") {
            Some(Value::Sequence(groups)) => groups
                .iter_mut()
                .find(|g| item_name(g) == Some(group_name.as_str())),
            _ => None,
        };
        let Some(group) = group else {
            warnings.push(MergeWarning::UnknownGroup {
                layer: layer.into(),
                group: group_name,
            });
            continue;
        };

        for (k, v) in edit {
            match group.get_mut(&k) {
                Some(existing) => deep_merge(existing, v),
                None => {
                    if let Value::Mapping(g) = group {
                        g.insert(k, v);
                    }
                }
            }
        }
        if remove.is_empty() && pre.is_empty() && app.is_empty() {
            continue;
        }
        if group_proxies(group).is_none() {
            if let Value::Mapping(g) = group {
                g.insert("proxies".into(), Value::Sequence(Vec::new()));
            }
        }
        let Some(list) = group_proxies(group) else {
            continue;
        };
        for pattern in &remove {
            let before = list.len();
            list.retain(|p| !p.as_str().is_some_and(|p| pattern.matches_name(p)));
            if list.len() == before {
                warnings.push(pattern.unused(layer, MODIFY_GROUPS));
            }
        }
        let added: HashSet<&str> = pre.iter().chain(&app).filter_map(Value::as_str).collect();
        let kept: Vec<Value> = list
            .drain(..)
            .filter(|p| !p.as_str().is_some_and(|p| added.contains(p)))
            .collect();
        list.extend(pre.iter().cloned());
        list.extend(kept);
        list.extend(app.iter().cloned());
        ensure_not_empty(group, warnings);
    }
    Ok(())
}

fn into_mapping(v: Value, what: &str) -> Result<Mapping, MergeError> {
    match v {
        Value::Mapping(m) => Ok(m),
        _ => Err(MergeError::NotAMapping(what.into())),
    }
}

fn take_list_from(m: &mut Mapping, key: &str, layer: &str) -> Result<Vec<Value>, MergeError> {
    match m.remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(s)) => Ok(s),
        Some(_) => Err(wrong_type(layer, key, "list")),
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

fn apply_forced(cfg: &mut Mapping, forced: &Forced, warnings: &mut Vec<MergeWarning>) {
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
    cfg.insert("allow-lan".into(), forced.allow_lan.into());
    if !forced.allow_lan {
        close_to_network(cfg, warnings);
    }

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

/// `allow-lan: false` only covers the main ports. Listeners and the DNS
/// server bind where they are told, so keep those on loopback too.
fn close_to_network(cfg: &mut Mapping, warnings: &mut Vec<MergeWarning>) {
    if let Some(Value::Sequence(listeners)) = cfg.get_mut("listeners") {
        listeners.retain(|l| {
            // mihomo listens on all interfaces when `listen` is missing.
            let listen = l.get("listen").and_then(Value::as_str).unwrap_or("0.0.0.0");
            if is_loopback(listen) {
                return true;
            }
            warnings.push(MergeWarning::ExposedListener {
                name: item_name(l).unwrap_or("?").to_owned(),
                listen: listen.to_owned(),
            });
            false
        });
    }
    if let Some(Value::Mapping(dns)) = cfg.get_mut("dns") {
        if let Some(listen) = dns.get("listen").and_then(Value::as_str).map(str::to_owned) {
            let (host, port) = split_host_port(&listen);
            if !is_loopback(host) {
                let local = match port {
                    Some(port) => format!("127.0.0.1:{port}"),
                    None => "127.0.0.1".to_owned(),
                };
                dns.insert("listen".into(), local.into());
                warnings.push(MergeWarning::ExposedDns { listen });
            }
        }
    }
}

/// `host:port`, `[v6]:port`, `:port` or a bare host.
fn split_host_port(addr: &str) -> (&str, Option<&str>) {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            return (host, tail.strip_prefix(':'));
        }
    }
    match addr.rsplit_once(':') {
        // A bare IPv6 address has several colons and no port.
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (addr, None),
    }
}

fn is_loopback(host: &str) -> bool {
    let host = split_host_port(host).0;
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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

/// Check targets, drop malformed rules and keep one `MATCH`, last.
fn finish_rules(cfg: &Mapping, rules: Vec<Rule>, warnings: &mut Vec<MergeWarning>) -> Vec<Value> {
    let mut known: HashSet<&str> = BUILTIN_TARGETS.iter().copied().collect();
    for key in ["proxies", "proxy-groups"] {
        if let Some(Value::Sequence(items)) = cfg.get(key) {
            known.extend(items.iter().filter_map(item_name));
        }
    }
    // Proxies from providers are only known at runtime, so don't rewrite blindly.
    let has_providers =
        matches!(cfg.get("proxy-providers"), Some(Value::Mapping(m)) if !m.is_empty());

    let mut out = Vec::new();
    let mut matches: Vec<(String, usize)> = Vec::new();
    for (raw, layer) in rules {
        let Some(text) = raw.as_str() else {
            warnings.push(MergeWarning::MalformedRule {
                rule: format!("{raw:?}"),
            });
            continue;
        };
        let mut parts = split_rule(text);
        let kind = parts[0].to_ascii_uppercase();
        let Some(idx) = target_index(&kind) else {
            out.push(raw);
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
            matches.push((rule, layer));
        } else {
            out.push(Value::String(rule));
        }
    }

    // The MATCH of the latest layer wins: the user's beats the subscription's.
    if let Some(winner) = matches
        .iter()
        .enumerate()
        .max_by_key(|(i, (_, layer))| (*layer, std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
    {
        let (rule, _) = matches.remove(winner);
        out.push(Value::String(rule));
    }
    warnings.extend(
        matches
            .into_iter()
            .map(|(rule, _)| MergeWarning::ExtraMatch { rule }),
    );
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
            allow_lan: false,
        }
    }

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    fn layer(name: &str, s: &str) -> Layer {
        Layer {
            name: name.into(),
            value: yaml(s),
        }
    }

    fn one(value: Value) -> Vec<Layer> {
        vec![Layer {
            name: "overlay.yaml".into(),
            value,
        }]
    }

    fn group_members(v: &Value, group: &str) -> Vec<String> {
        v["proxy-groups"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|g| g["name"] == group)
            .unwrap()["proxies"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_owned())
            .collect()
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
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
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
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
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
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
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
        let out = merge(yaml(&sub), one(overlay), &forced(false)).unwrap();
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
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
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
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
        assert_eq!(names(&out.config, "proxies"), ["b", "a", "c"]);
        assert_eq!(out.config["proxies"][0]["type"], "vless");
        assert_eq!(names(&out.config, "proxy-groups"), ["Mine", "Proxy"]);
    }

    #[test]
    fn other_keys_deep_merge() {
        let sub = format!("{SUB}\ndns: {{enable: false, nameserver: [1.1.1.1]}}\nlog-level: info");
        let overlay = yaml("dns: {nameserver: [9.9.9.9], ipv6: true}\nlog-level: debug");
        let out = merge(yaml(&sub), one(overlay), &forced(false)).unwrap();
        let dns = &out.config["dns"];
        assert_eq!(dns["enable"], false);
        assert_eq!(dns["ipv6"], true);
        assert_eq!(dns["nameserver"], yaml("[9.9.9.9]"));
        assert_eq!(out.config["log-level"], "debug");
    }

    #[test]
    fn forced_settings_win_and_remove_tcp_controller() {
        let overlay = yaml("mixed-port: 1111\nexternal-controller: 127.0.0.1:9090");
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
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
        let out = merge(yaml(SUB), one(overlay), &forced(true)).unwrap();
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
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
        assert_eq!(rules(&out.config).len(), 3);
        assert_eq!(out.warnings.len(), 2);
    }

    #[test]
    fn rejects_non_mapping_input() {
        assert!(merge(yaml("[1, 2]"), vec![], &forced(false)).is_err());
        assert!(merge(yaml(SUB), one(yaml("prepend-rules: oops")), &forced(false)).is_err());
    }

    #[test]
    fn delete_rules_by_fields_and_regex() {
        let overlay = yaml(
            r#"
delete-rules:
  - "DOMAIN-SUFFIX,google.com"
  - "/^GEOIP,/"
  - "domain,nothing.example"
"#,
        );
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
        assert_eq!(rules(&out.config), ["MATCH,Proxy"]);
        assert_eq!(
            out.warnings,
            [MergeWarning::Unused {
                layer: "overlay.yaml".into(),
                key: DELETE_RULES,
                entry: "domain,nothing.example".into(),
            }]
        );
    }

    #[test]
    fn field_patterns_do_not_match_partial_values() {
        let p = Pattern::Literal("DOMAIN-SUFFIX,google.com".into());
        assert!(p.matches_rule("domain-suffix,google.com,Proxy,no-resolve"));
        assert!(!p.matches_rule("DOMAIN-SUFFIX,google.com.evil,Proxy"));
        assert!(!p.matches_rule("DOMAIN-SUFFIX,Google.com,Proxy"));
    }

    #[test]
    fn replace_targets_only_touches_earlier_rules() {
        let overlay = yaml(
            r#"
replace-rule-targets: {Proxy: DIRECT, Missing: REJECT}
prepend-rules: ["DOMAIN,mine.example,Proxy"]
"#,
        );
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
        assert_eq!(
            rules(&out.config),
            [
                "DOMAIN,mine.example,Proxy",
                "DOMAIN-SUFFIX,google.com,DIRECT",
                "GEOIP,CN,DIRECT",
                "MATCH,DIRECT"
            ]
        );
        assert!(matches!(
            &out.warnings[..],
            [MergeWarning::Unused { key: REPLACE_TARGETS, entry, .. }] if entry == "Missing"
        ));
    }

    #[test]
    fn deleted_proxies_leave_groups_valid() {
        let out = merge(
            yaml(SUB),
            one(yaml("delete-proxies: [\"/^a$/\"]")),
            &forced(false),
        )
        .unwrap();
        assert_eq!(names(&out.config, "proxies"), ["b"]);
        assert_eq!(group_members(&out.config, "Proxy"), ["b"]);

        let out = merge(
            yaml(SUB),
            one(yaml("delete-proxies: [a, b]")),
            &forced(false),
        )
        .unwrap();
        assert_eq!(group_members(&out.config, "Proxy"), ["DIRECT"]);
        assert!(out.warnings.contains(&MergeWarning::EmptyGroup {
            group: "Proxy".into()
        }));
    }

    #[test]
    fn deleted_group_rules_fall_back_to_direct() {
        let out = merge(
            yaml(SUB),
            one(yaml("delete-proxy-groups: [Proxy]")),
            &forced(false),
        )
        .unwrap();
        assert!(names(&out.config, "proxy-groups").is_empty());
        assert_eq!(rules(&out.config)[0], "DOMAIN-SUFFIX,google.com,DIRECT");
    }

    #[test]
    fn modify_groups_edits_members_and_keys() {
        let overlay = yaml(
            r#"
append-proxies: [{name: c, type: ss}]
modify-proxy-groups:
  Proxy:
    prepend-proxies: [c]
    remove-proxies: [a]
    type: url-test
    interval: 300
  Nope:
    append-proxies: [c]
"#,
        );
        let out = merge(yaml(SUB), one(overlay), &forced(false)).unwrap();
        assert_eq!(group_members(&out.config, "Proxy"), ["c", "b"]);
        let group = &out.config["proxy-groups"][0];
        assert_eq!(group["type"], "url-test");
        assert_eq!(group["interval"], 300);
        assert_eq!(
            out.warnings,
            [MergeWarning::UnknownGroup {
                layer: "overlay.yaml".into(),
                group: "Nope".into()
            }]
        );
    }

    #[test]
    fn layers_apply_in_order() {
        let remote = layer(
            "remote",
            r#"
prepend-rules: ["DOMAIN,remote.example,REJECT", "MATCH,DIRECT"]
append-proxies: [{name: r, type: ss}]
"#,
        );
        // The local overlay can undo what the remote one added.
        let local = layer(
            "overlay.yaml",
            r#"
delete-rules: ["DOMAIN,remote.example"]
delete-proxies: [r]
prepend-rules: ["DOMAIN,local.example,DIRECT"]
"#,
        );
        let out = merge(yaml(SUB), vec![remote, local], &forced(false)).unwrap();
        assert_eq!(
            rules(&out.config),
            [
                "DOMAIN,local.example,DIRECT",
                "DOMAIN-SUFFIX,google.com,Proxy",
                "GEOIP,CN,DIRECT",
                "MATCH,DIRECT"
            ]
        );
        assert_eq!(names(&out.config, "proxies"), ["a", "b"]);
        assert_eq!(
            out.warnings,
            [MergeWarning::ExtraMatch {
                rule: "MATCH,Proxy".into()
            }]
        );
    }

    #[test]
    fn bad_regex_is_an_error() {
        let err = merge(
            yaml(SUB),
            one(yaml("delete-rules: [\"/(/\"]")),
            &forced(false),
        )
        .unwrap_err();
        assert!(matches!(err, MergeError::BadPattern { .. }));
    }

    #[test]
    fn lan_access_is_closed_unless_allowed() {
        let sub = format!(
            "{SUB}allow-lan: true\nbind-address: '*'\n\
             dns: {{enable: true, listen: '0.0.0.0:1053'}}\n\
             listeners:\n  \
               - {{name: open, type: socks, port: 10808}}\n  \
               - {{name: lan, type: mixed, port: 10809, listen: 192.168.1.5}}\n  \
               - {{name: local, type: mixed, port: 10810, listen: 127.0.0.1}}\n"
        );
        let out = merge(yaml(&sub), vec![], &forced(false)).unwrap();
        let c = &out.config;
        assert_eq!(c["allow-lan"], false);
        assert_eq!(c["dns"]["listen"], "127.0.0.1:1053");
        assert_eq!(names(c, "listeners"), ["local"]);
        assert_eq!(
            out.warnings
                .iter()
                .filter(|w| matches!(
                    w,
                    MergeWarning::ExposedListener { .. } | MergeWarning::ExposedDns { .. }
                ))
                .count(),
            3
        );

        // An overlay cannot open it either; only the user's setting can.
        let out = merge(yaml(&sub), one(yaml("allow-lan: true")), &forced(false)).unwrap();
        assert_eq!(out.config["allow-lan"], false);
        let open = Forced {
            allow_lan: true,
            ..forced(false)
        };
        let out = merge(yaml(&sub), vec![], &open).unwrap();
        assert_eq!(out.config["allow-lan"], true);
        assert_eq!(out.config["dns"]["listen"], "0.0.0.0:1053");
        assert_eq!(names(&out.config, "listeners").len(), 3);
    }

    #[test]
    fn loopback_detection() {
        for local in [
            "127.0.0.1",
            "127.0.0.1:53",
            "[::1]:53",
            "::1",
            "localhost:1053",
        ] {
            assert!(is_loopback(local), "{local}");
        }
        for open in ["0.0.0.0:53", ":53", "192.168.1.5", "[::]:53", "::"] {
            assert!(!is_loopback(open), "{open}");
        }
    }
}
