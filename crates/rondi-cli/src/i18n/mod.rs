//! Pure-Rust Tier 1 localization for the `rondi`, `rrdtool`, and `rrdcached`
//! command-line front ends.
//!
//! Under `Locale::En` (including `C`, `POSIX`, and unset locale environment
//! variables), every compatibility output is returned byte-for-byte identical
//! to the original RRDtool 1.11.0 text.

mod catalogs;

use rondi::{Locale, TimeLocale};
use std::borrow::Cow;

/// All user-facing CLI strings and command help bodies for one locale.
pub(crate) struct Catalog {
    // rrdtool front-end usage & banner
    pub rrdtool_banner: &'static str,
    pub rrdtool_valid_commands: &'static str,
    pub rrdtool_valid_remote_commands: &'static str,
    pub rrdtool_notice: &'static str,

    // rrdtool front-end & option errors
    pub rrdtool_unknown_function: &'static str,
    pub rrdtool_unknown_function_bare: &'static str,
    pub opt_invalid: &'static str,
    pub opt_requires_arg: &'static str,
    pub opt_requires_an_arg: &'static str,
    pub opt_takes_no_args: &'static str,

    // pipe mode errors
    pub pipe_not_enough_args: &'static str,
    pub pipe_creating_args: &'static str,
    pub pipe_invalid_param_count: &'static str,

    // alias front-end usage & errors (rrdupdate, rrdcreate, rrdinfo)
    pub alias_usage_update: &'static str,
    pub alias_usage_create: &'static str,
    pub alias_usage_info: &'static str,
    pub alias_not_enough_args: &'static str,
    pub alias_need_rrd_name: &'static str,
    pub alias_info_usage_error: &'static str,

    // rrdcached usage & front-end errors
    pub rrdcached_usage: &'static str,
    pub rrdcached_missing_arg: &'static str,
    pub rrdcached_invalid_alloc: &'static str,
    pub rrdcached_map_group: &'static str,
    pub rrdcached_map_user: &'static str,
    pub rrdcached_invalid_mode: &'static str,
    pub rrdcached_invalid_threads: &'static str,
    pub rrdcached_unrecognized_loglevel: &'static str,
    pub rrdcached_invalid_interval: &'static str,
    pub rrdcached_write_interval: &'static str,
    pub rrdcached_flush_interval: &'static str,
    pub rrdcached_write_jitter: &'static str,
    pub rrdcached_interval_not_number: &'static str,
    pub rrdcached_interval_trailing_garbage: &'static str,
    pub rrdcached_interval_not_positive: &'static str,
    pub rrdcached_bad_permission: &'static str,

    // native `rondi` CLI help & errors
    pub native_about: &'static str,
    pub native_arg_lang: &'static str,
    pub native_arg_socket: &'static str,
    pub native_arg_root: &'static str,
    pub native_arg_max_rows: &'static str,
    pub native_arg_idempotency_window: &'static str,
    pub native_cmd_server: &'static str,
    pub native_cmd_server_listen: &'static str,
    pub native_cmd_server_queue_capacity: &'static str,
    pub native_cmd_create: &'static str,
    pub native_cmd_create_name: &'static str,
    pub native_cmd_create_step: &'static str,
    pub native_cmd_create_heartbeat: &'static str,
    pub native_cmd_create_rows: &'static str,
    pub native_cmd_create_start: &'static str,
    pub native_cmd_update: &'static str,
    pub native_cmd_update_name: &'static str,
    pub native_cmd_update_timestamp: &'static str,
    pub native_cmd_update_value: &'static str,
    pub native_cmd_fetch: &'static str,
    pub native_cmd_fetch_name: &'static str,
    pub native_cmd_health: &'static str,
    pub native_err_invalid_lang: &'static str,
    pub native_err_server_not_prepared: &'static str,
    pub native_err_server_client_request: &'static str,
    pub native_err_server_http: &'static str,

    // 22 rrdtool command help bodies
    pub help_cd: &'static str,
    pub help_create: &'static str,
    pub help_dump: &'static str,
    pub help_fetch: &'static str,
    pub help_first: &'static str,
    pub help_flushcached: &'static str,
    pub help_graph: &'static str,
    pub help_graphv: &'static str,
    pub help_info: &'static str,
    pub help_last: &'static str,
    pub help_lastupdate: &'static str,
    pub help_list: &'static str,
    pub help_ls: &'static str,
    pub help_mkdir: &'static str,
    pub help_pwd: &'static str,
    pub help_quit: &'static str,
    pub help_resize: &'static str,
    pub help_restore: &'static str,
    pub help_tune: &'static str,
    pub help_update: &'static str,
    pub help_updatev: &'static str,
    pub help_xport: &'static str,
}

/// All 22 `rrdtool` help topics.
#[cfg(test)]
pub(crate) const HELP_TOPICS: &[&str] = &[
    "cd",
    "create",
    "dump",
    "fetch",
    "first",
    "flushcached",
    "graph",
    "graphv",
    "info",
    "last",
    "lastupdate",
    "list",
    "ls",
    "mkdir",
    "pwd",
    "quit",
    "resize",
    "restore",
    "tune",
    "update",
    "updatev",
    "xport",
];

// Byte-for-byte English compatibility pages.
#[cfg(test)]
const EN_BANNER: &str = "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n";
#[cfg(test)]
const EN_VALID_COMMANDS: &str = "Valid commands: create, update, updatev, graph, graphv,  dump, restore,\n\t\tlast, lastupdate, first, info, list, fetch, tune,\n\t\tresize, xport, flushcached\n";
#[cfg(test)]
const EN_VALID_REMOTE_COMMANDS: &str = "\nValid remote commands: quit, ls, cd, mkdir, pwd\n";
#[cfg(test)]
const EN_NOTICE: &str = "\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n";

pub(crate) const EN_USAGE: &str = concat!(
    "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n",
    "Valid commands: create, update, updatev, graph, graphv,  dump, restore,\n\t\tlast, lastupdate, first, info, list, fetch, tune,\n\t\tresize, xport, flushcached\n",
    "\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n"
);

pub(crate) const EN_PIPE_USAGE: &str = concat!(
    "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n",
    "Valid commands: create, update, updatev, graph, graphv,  dump, restore,\n\t\tlast, lastupdate, first, info, list, fetch, tune,\n\t\tresize, xport, flushcached\n",
    "\nValid remote commands: quit, ls, cd, mkdir, pwd\n",
    "\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n"
);

/// Resolves the active CLI messages locale from POSIX environment variables
/// (`LC_ALL`, then `LC_MESSAGES`, then `LANG`), defaulting to [`Locale::En`].
pub(crate) fn messages_locale() -> Locale {
    first_non_empty_env(&["LC_ALL", "LC_MESSAGES", "LANG"])
        .as_deref()
        .map_or(Locale::En, Locale::from_posix_env)
}

/// Resolves the active `LC_TIME` locale from POSIX environment variables
/// (`LC_ALL`, then `LC_TIME`, then `LANG`), defaulting to [`TimeLocale::En`].
pub(crate) fn time_locale() -> TimeLocale {
    first_non_empty_env(&["LC_ALL", "LC_TIME", "LANG"])
        .as_deref()
        .map_or(TimeLocale::En, TimeLocale::from_posix_env)
}

fn first_non_empty_env(vars: &[&str]) -> Option<String> {
    for name in vars {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// Returns the static message catalog for `locale`.
pub(crate) const fn catalog(locale: Locale) -> &'static Catalog {
    catalogs::get(locale)
}

/// Formats a template containing `{0}`.
pub(crate) fn fmt1(template: &str, a0: &str) -> String {
    template.replace("{0}", a0)
}

/// Formats a template containing `{0}` and `{1}`.
pub(crate) fn fmt2(template: &str, a0: &str, a1: &str) -> String {
    template.replace("{0}", a0).replace("{1}", a1)
}

/// Formats a template containing `{0}`, `{1}`, and `{2}`.
pub(crate) fn fmt3(template: &str, a0: &str, a1: &str, a2: &str) -> String {
    template
        .replace("{0}", a0)
        .replace("{1}", a1)
        .replace("{2}", a2)
}

/// Comma-separated list of all 20 supported locale codes.
pub(crate) fn supported_locale_codes() -> String {
    Locale::ALL
        .iter()
        .map(|locale| locale.code())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Generic `rrdtool` usage text.
pub(crate) fn rrdtool_usage(locale: Locale) -> Cow<'static, str> {
    if locale == Locale::En {
        return Cow::Borrowed(EN_USAGE);
    }
    let cat = catalog(locale);
    Cow::Owned(format!(
        "{}{}{}",
        cat.rrdtool_banner, cat.rrdtool_valid_commands, cat.rrdtool_notice
    ))
}

/// Pipe-mode `rrdtool -` usage text.
pub(crate) fn rrdtool_pipe_usage(locale: Locale) -> Cow<'static, str> {
    if locale == Locale::En {
        return Cow::Borrowed(EN_PIPE_USAGE);
    }
    let cat = catalog(locale);
    Cow::Owned(format!(
        "{}{}{}{}",
        cat.rrdtool_banner,
        cat.rrdtool_valid_commands,
        cat.rrdtool_valid_remote_commands,
        cat.rrdtool_notice
    ))
}

/// Exact English `rrdtool` help page for `topic` (byte-for-byte from `help/*.txt`).
pub(crate) fn en_help_page(topic: &str) -> Option<&'static str> {
    Some(match topic {
        "cd" => include_str!("../help/cd.txt"),
        "create" => include_str!("../help/create.txt"),
        "dump" => include_str!("../help/dump.txt"),
        "fetch" => include_str!("../help/fetch.txt"),
        "first" => include_str!("../help/first.txt"),
        "flushcached" => include_str!("../help/flushcached.txt"),
        "graph" => include_str!("../help/graph.txt"),
        "graphv" => include_str!("../help/graphv.txt"),
        "info" => include_str!("../help/info.txt"),
        "last" => include_str!("../help/last.txt"),
        "lastupdate" => include_str!("../help/lastupdate.txt"),
        "list" => include_str!("../help/list.txt"),
        "ls" => include_str!("../help/ls.txt"),
        "mkdir" => include_str!("../help/mkdir.txt"),
        "pwd" => include_str!("../help/pwd.txt"),
        "quit" => include_str!("../help/quit.txt"),
        "resize" => include_str!("../help/resize.txt"),
        "restore" => include_str!("../help/restore.txt"),
        "tune" => include_str!("../help/tune.txt"),
        "update" => include_str!("../help/update.txt"),
        "updatev" => include_str!("../help/updatev.txt"),
        "xport" => include_str!("../help/xport.txt"),
        _ => return None,
    })
}

/// Help body for `topic` in `cat`.
pub(crate) fn help_body(cat: &'static Catalog, topic: &str) -> Option<&'static str> {
    Some(match topic {
        "cd" => cat.help_cd,
        "create" => cat.help_create,
        "dump" => cat.help_dump,
        "fetch" => cat.help_fetch,
        "first" => cat.help_first,
        "flushcached" => cat.help_flushcached,
        "graph" => cat.help_graph,
        "graphv" => cat.help_graphv,
        "info" => cat.help_info,
        "last" => cat.help_last,
        "lastupdate" => cat.help_lastupdate,
        "list" => cat.help_list,
        "ls" => cat.help_ls,
        "mkdir" => cat.help_mkdir,
        "pwd" => cat.help_pwd,
        "quit" => cat.help_quit,
        "resize" => cat.help_resize,
        "restore" => cat.help_restore,
        "tune" => cat.help_tune,
        "update" => cat.help_update,
        "updatev" => cat.help_updatev,
        "xport" => cat.help_xport,
        _ => return None,
    })
}

/// Full `rrdtool` help page for `topic` in `locale`.
pub(crate) fn rrdtool_help(topic: &str, locale: Locale) -> Option<Cow<'static, str>> {
    if locale == Locale::En {
        return en_help_page(topic).map(Cow::Borrowed);
    }
    let cat = catalog(locale);
    let body = help_body(cat, topic)?;
    Some(Cow::Owned(format!(
        "{}{}{}",
        cat.rrdtool_banner, body, cat.rrdtool_notice
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_placeholders(text: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == b'{' && bytes[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'}' {
                    found.push(&text[i..=j]);
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
        found
    }

    fn extract_cli_flags(text: &str) -> Vec<String> {
        let mut flags = Vec::new();
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let is_word = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '-';
        let mut idx = 0;
        while idx < chars.len() {
            let (byte_pos, ch) = chars[idx];
            if ch == '-' && (idx == 0 || !is_word(chars[idx - 1].1)) {
                if idx + 2 < chars.len()
                    && chars[idx + 1].1 == '-'
                    && chars[idx + 2].1.is_ascii_lowercase()
                {
                    let mut end_idx = idx + 2;
                    while end_idx < chars.len()
                        && (chars[end_idx].1.is_ascii_lowercase()
                            || chars[end_idx].1.is_ascii_digit()
                            || chars[end_idx].1 == '-')
                    {
                        end_idx += 1;
                    }
                    let end_byte = if end_idx < chars.len() {
                        chars[end_idx].0
                    } else {
                        text.len()
                    };
                    flags.push(text[byte_pos..end_byte].to_owned());
                    idx = end_idx;
                    continue;
                }
                if idx + 1 < chars.len()
                    && chars[idx + 1].1.is_ascii_alphabetic()
                    && (idx + 2 >= chars.len() || !is_word(chars[idx + 2].1))
                {
                    let end_byte = if idx + 2 < chars.len() {
                        chars[idx + 2].0
                    } else {
                        text.len()
                    };
                    flags.push(text[byte_pos..end_byte].to_owned());
                    idx += 2;
                    continue;
                }
            }
            idx += 1;
        }
        flags.sort();
        flags
    }

    fn all_fields(c: &'static Catalog) -> Vec<(&'static str, &'static str)> {
        vec![
            ("rrdtool_banner", c.rrdtool_banner),
            ("rrdtool_valid_commands", c.rrdtool_valid_commands),
            (
                "rrdtool_valid_remote_commands",
                c.rrdtool_valid_remote_commands,
            ),
            ("rrdtool_notice", c.rrdtool_notice),
            ("rrdtool_unknown_function", c.rrdtool_unknown_function),
            (
                "rrdtool_unknown_function_bare",
                c.rrdtool_unknown_function_bare,
            ),
            ("opt_invalid", c.opt_invalid),
            ("opt_requires_arg", c.opt_requires_arg),
            ("opt_requires_an_arg", c.opt_requires_an_arg),
            ("opt_takes_no_args", c.opt_takes_no_args),
            ("pipe_not_enough_args", c.pipe_not_enough_args),
            ("pipe_creating_args", c.pipe_creating_args),
            ("pipe_invalid_param_count", c.pipe_invalid_param_count),
            ("alias_usage_update", c.alias_usage_update),
            ("alias_usage_create", c.alias_usage_create),
            ("alias_usage_info", c.alias_usage_info),
            ("alias_not_enough_args", c.alias_not_enough_args),
            ("alias_need_rrd_name", c.alias_need_rrd_name),
            ("alias_info_usage_error", c.alias_info_usage_error),
            ("rrdcached_usage", c.rrdcached_usage),
            ("rrdcached_missing_arg", c.rrdcached_missing_arg),
            ("rrdcached_invalid_alloc", c.rrdcached_invalid_alloc),
            ("rrdcached_map_group", c.rrdcached_map_group),
            ("rrdcached_map_user", c.rrdcached_map_user),
            ("rrdcached_invalid_mode", c.rrdcached_invalid_mode),
            ("rrdcached_invalid_threads", c.rrdcached_invalid_threads),
            (
                "rrdcached_unrecognized_loglevel",
                c.rrdcached_unrecognized_loglevel,
            ),
            ("rrdcached_invalid_interval", c.rrdcached_invalid_interval),
            ("rrdcached_write_interval", c.rrdcached_write_interval),
            ("rrdcached_flush_interval", c.rrdcached_flush_interval),
            ("rrdcached_write_jitter", c.rrdcached_write_jitter),
            (
                "rrdcached_interval_not_number",
                c.rrdcached_interval_not_number,
            ),
            (
                "rrdcached_interval_trailing_garbage",
                c.rrdcached_interval_trailing_garbage,
            ),
            (
                "rrdcached_interval_not_positive",
                c.rrdcached_interval_not_positive,
            ),
            ("rrdcached_bad_permission", c.rrdcached_bad_permission),
            ("native_about", c.native_about),
            ("native_arg_lang", c.native_arg_lang),
            ("native_arg_socket", c.native_arg_socket),
            ("native_arg_root", c.native_arg_root),
            ("native_arg_max_rows", c.native_arg_max_rows),
            (
                "native_arg_idempotency_window",
                c.native_arg_idempotency_window,
            ),
            ("native_cmd_server", c.native_cmd_server),
            ("native_cmd_server_listen", c.native_cmd_server_listen),
            (
                "native_cmd_server_queue_capacity",
                c.native_cmd_server_queue_capacity,
            ),
            ("native_cmd_create", c.native_cmd_create),
            ("native_cmd_create_name", c.native_cmd_create_name),
            ("native_cmd_create_step", c.native_cmd_create_step),
            ("native_cmd_create_heartbeat", c.native_cmd_create_heartbeat),
            ("native_cmd_create_rows", c.native_cmd_create_rows),
            ("native_cmd_create_start", c.native_cmd_create_start),
            ("native_cmd_update", c.native_cmd_update),
            ("native_cmd_update_name", c.native_cmd_update_name),
            ("native_cmd_update_timestamp", c.native_cmd_update_timestamp),
            ("native_cmd_update_value", c.native_cmd_update_value),
            ("native_cmd_fetch", c.native_cmd_fetch),
            ("native_cmd_fetch_name", c.native_cmd_fetch_name),
            ("native_cmd_health", c.native_cmd_health),
            ("native_err_invalid_lang", c.native_err_invalid_lang),
            (
                "native_err_server_not_prepared",
                c.native_err_server_not_prepared,
            ),
            (
                "native_err_server_client_request",
                c.native_err_server_client_request,
            ),
            ("native_err_server_http", c.native_err_server_http),
            ("help_cd", c.help_cd),
            ("help_create", c.help_create),
            ("help_dump", c.help_dump),
            ("help_fetch", c.help_fetch),
            ("help_first", c.help_first),
            ("help_flushcached", c.help_flushcached),
            ("help_graph", c.help_graph),
            ("help_graphv", c.help_graphv),
            ("help_info", c.help_info),
            ("help_last", c.help_last),
            ("help_lastupdate", c.help_lastupdate),
            ("help_list", c.help_list),
            ("help_ls", c.help_ls),
            ("help_mkdir", c.help_mkdir),
            ("help_pwd", c.help_pwd),
            ("help_quit", c.help_quit),
            ("help_resize", c.help_resize),
            ("help_restore", c.help_restore),
            ("help_tune", c.help_tune),
            ("help_update", c.help_update),
            ("help_updatev", c.help_updatev),
            ("help_xport", c.help_xport),
        ]
    }

    #[test]
    fn english_catalog_is_byte_identical_to_original_help_and_usage() {
        let en = catalog(Locale::En);
        assert_eq!(en.rrdtool_banner, EN_BANNER);
        assert_eq!(en.rrdtool_valid_commands, EN_VALID_COMMANDS);
        assert_eq!(en.rrdtool_valid_remote_commands, EN_VALID_REMOTE_COMMANDS);
        assert_eq!(en.rrdtool_notice, EN_NOTICE);
        assert_eq!(rrdtool_usage(Locale::En).as_ref(), EN_USAGE);
        assert_eq!(rrdtool_pipe_usage(Locale::En).as_ref(), EN_PIPE_USAGE);
        for &topic in HELP_TOPICS {
            let expected = en_help_page(topic).unwrap();
            let actual = rrdtool_help(topic, Locale::En).unwrap();
            assert_eq!(actual.as_ref(), expected, "topic={topic}");
            let assembled = format!(
                "{}{}{}",
                en.rrdtool_banner,
                help_body(en, topic).unwrap(),
                en.rrdtool_notice
            );
            assert_eq!(assembled, expected, "assembled topic={topic}");
        }
    }

    #[test]
    fn all_20_catalogs_and_440_help_pages_pass_parity_checks() {
        let en = catalog(Locale::En);
        let en_fields = all_fields(en);
        assert_eq!(Locale::ALL.len(), 20);
        assert_eq!(HELP_TOPICS.len(), 22);
        let mut help_pages_checked = 0;
        for &locale in Locale::ALL {
            let cat = catalog(locale);
            let fields = all_fields(cat);
            assert_eq!(fields.len(), en_fields.len());
            for ((en_key, en_val), (key, val)) in en_fields.iter().zip(fields.iter()) {
                assert_eq!(en_key, key);
                assert!(!val.trim().is_empty(), "{locale}.{key} is empty");
                assert_eq!(
                    extract_placeholders(val),
                    extract_placeholders(en_val),
                    "placeholder parity mismatch in {locale}.{key}"
                );
                if key.starts_with("help_")
                    || matches!(
                        *key,
                        "rrdcached_usage"
                            | "alias_usage_update"
                            | "alias_usage_create"
                            | "alias_usage_info"
                            | "alias_info_usage_error"
                    )
                {
                    assert_eq!(
                        extract_cli_flags(val),
                        extract_cli_flags(en_val),
                        "CLI flag parity mismatch in {locale}.{key}"
                    );
                }
            }
            for &topic in HELP_TOPICS {
                let page = rrdtool_help(topic, locale).unwrap();
                assert!(page.starts_with(cat.rrdtool_banner), "{locale} {topic}");
                assert!(page.ends_with(cat.rrdtool_notice), "{locale} {topic}");
                help_pages_checked += 1;
            }
            if locale != Locale::En {
                assert_ne!(
                    cat.native_about, en.native_about,
                    "{locale}.native_about was not translated"
                );
                assert_ne!(
                    cat.rrdtool_unknown_function, en.rrdtool_unknown_function,
                    "{locale}.rrdtool_unknown_function was not translated"
                );
                assert_ne!(
                    cat.opt_invalid, en.opt_invalid,
                    "{locale}.opt_invalid was not translated"
                );
            }
        }
        assert_eq!(help_pages_checked, 440);
    }
}
