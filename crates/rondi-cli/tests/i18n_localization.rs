#![cfg(unix)]

use rondi::{Locale, TimeLocale};
use std::path::{Path, PathBuf};
use std::process::Command;

fn rondi_bin() -> &'static str {
    env!("CARGO_BIN_EXE_rondi")
}

fn symlink_alias(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::os::unix::fs::symlink(rondi_bin(), &path).unwrap();
    path
}

fn clean_locale_cmd(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env_remove("LC_TIME")
        .env_remove("LC_NUMERIC")
        .env_remove("LANG")
        .env_remove("LANGUAGE")
        .env("TZ", "UTC");
    cmd
}

#[test]
fn native_rondi_lang_flag_and_posix_env_localize_help() {
    let en = clean_locale_cmd(rondi_bin())
        .args(["--help"])
        .output()
        .unwrap();
    assert!(en.status.success());
    let en_out = String::from_utf8(en.stdout).unwrap();
    assert!(en_out.contains("Round-robin time-series storage"));
    assert!(en_out.contains("--lang"));

    let ja = clean_locale_cmd(rondi_bin())
        .args(["--lang", "ja", "--help"])
        .output()
        .unwrap();
    assert!(ja.status.success());
    let ja_out = String::from_utf8(ja.stdout).unwrap();
    assert_ne!(ja_out, en_out);
    assert!(ja_out.contains("--lang"));
    assert!(
        ja_out
            .chars()
            .any(|c| ('\u{3040}'..='\u{9FFF}').contains(&c))
    );

    let de_create = clean_locale_cmd(rondi_bin())
        .args(["--lang", "de", "create", "--help"])
        .output()
        .unwrap();
    assert!(de_create.status.success());
    let de_create_out = String::from_utf8(de_create.stdout).unwrap();
    assert!(de_create_out.contains("--step"));
    assert!(!de_create_out.contains("Base sampling interval in seconds."));

    let zh_cn = clean_locale_cmd(rondi_bin())
        .args(["--lang", "zh-CN", "--help"])
        .output()
        .unwrap();
    let zh_tw = clean_locale_cmd(rondi_bin())
        .args(["--lang", "zh-TW", "--help"])
        .output()
        .unwrap();
    assert!(zh_cn.status.success() && zh_tw.status.success());
    assert_ne!(zh_cn.stdout, zh_tw.stdout);

    let hi = clean_locale_cmd(rondi_bin())
        .args(["--lang", "hi", "--help"])
        .output()
        .unwrap();
    assert!(hi.status.success());
    let hi_out = String::from_utf8(hi.stdout).unwrap();
    assert!(
        hi_out
            .chars()
            .any(|c| ('\u{0900}'..='\u{097F}').contains(&c))
    );

    let ar = clean_locale_cmd(rondi_bin())
        .args(["--lang", "ar", "--help"])
        .output()
        .unwrap();
    assert!(ar.status.success());
    let ar_out = String::from_utf8(ar.stdout).unwrap();
    assert!(
        ar_out
            .chars()
            .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
    );

    let fr_env = clean_locale_cmd(rondi_bin())
        .env("LC_ALL", "fr_FR.UTF-8")
        .args(["--help"])
        .output()
        .unwrap();
    assert!(fr_env.status.success());
    assert_ne!(String::from_utf8(fr_env.stdout).unwrap(), en_out);

    let invalid = clean_locale_cmd(rondi_bin())
        .args(["--lang", "xx-INVALID", "--help"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("xx-INVALID"));
}

#[test]
fn rrdtool_default_c_posix_en_unset_are_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let rrdtool = symlink_alias(dir.path(), "rrdtool");

    let configs: &[(&str, Option<&str>)] = &[
        ("unset", None),
        ("C", Some("C")),
        ("POSIX", Some("POSIX")),
        ("en_US.UTF-8", Some("en_US.UTF-8")),
    ];
    for args in [
        vec![],
        vec!["help", "create"],
        vec!["help", "graph"],
        vec!["help", "dump"],
        vec!["boguscmd", "arg"],
    ] {
        let mut baseline = None;
        for (label, lc_all) in configs {
            let mut cmd = clean_locale_cmd(&rrdtool);
            if let Some(val) = lc_all {
                cmd.env("LC_ALL", val);
            }
            let out = cmd.args(&args).output().unwrap();
            let result = (out.status.code(), out.stdout, out.stderr);
            if let Some(expected) = &baseline {
                assert_eq!(&result, expected, "args={args:?} locale={label}");
            } else {
                baseline = Some(result);
            }
        }
        if args == vec!["help", "create"] {
            let expected = include_str!("../src/help/create.txt").as_bytes();
            assert_eq!(baseline.as_ref().unwrap().1.as_slice(), expected);
        }
    }
}

#[test]
fn rrdtool_honors_posix_locale_and_rejects_non_rrdtool_lang_flag() {
    let dir = tempfile::tempdir().unwrap();
    let rrdtool = symlink_alias(dir.path(), "rrdtool");

    // German help preserves all CLI flags and translates prose.
    let de = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "de_DE.UTF-8")
        .args(["help", "create"])
        .output()
        .unwrap();
    assert!(de.status.success());
    let de_out = String::from_utf8(de.stdout).unwrap();
    assert!(de_out.contains("--step|-s"));
    assert!(de_out.contains("--start|-b"));
    assert!(de_out.contains("--no-overwrite|-O"));
    assert!(de_out.contains("--daemon|-d"));
    assert_ne!(de_out, include_str!("../src/help/create.txt"));

    // Arabic usage under LC_ALL=ar_SA.UTF-8.
    let ar = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "ar_SA.UTF-8")
        .output()
        .unwrap();
    assert!(ar.status.success());
    let ar_out = String::from_utf8(ar.stdout).unwrap();
    assert!(
        ar_out
            .chars()
            .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c))
    );
    assert!(ar_out.contains("flushcached"));

    // Localized unknown function error.
    let ja_err = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "ja_JP.UTF-8")
        .args(["notacommand", "x"])
        .output()
        .unwrap();
    assert_eq!(ja_err.status.code(), Some(1));
    let ja_stderr = String::from_utf8(ja_err.stderr).unwrap();
    assert!(ja_stderr.starts_with("ERROR: "));
    assert!(ja_stderr.contains("notacommand"));
    assert!(!ja_stderr.contains("unknown function"));

    // POSIX precedence: LC_ALL=C overrides LC_MESSAGES and LANG.
    let c_override = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "C")
        .env("LC_MESSAGES", "de_DE.UTF-8")
        .env("LANG", "ja_JP.UTF-8")
        .args(["help", "create"])
        .output()
        .unwrap();
    assert_eq!(
        c_override.stdout,
        include_str!("../src/help/create.txt").as_bytes()
    );

    // POSIX precedence: LC_MESSAGES overrides LANG when LC_ALL is empty.
    let es = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "")
        .env("LC_MESSAGES", "es_ES.UTF-8")
        .env("LANG", "fr_FR.UTF-8")
        .args(["help", "last"])
        .output()
        .unwrap();
    let es_direct = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "es_ES.UTF-8")
        .args(["help", "last"])
        .output()
        .unwrap();
    assert_eq!(es.stdout, es_direct.stdout);

    // No non-RRDtool `--lang` flag on `rrdtool` compatibility personality.
    let no_lang = clean_locale_cmd(&rrdtool)
        .env("LC_ALL", "C")
        .args(["--lang", "ja"])
        .output()
        .unwrap();
    assert_eq!(no_lang.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&no_lang.stderr),
        "ERROR: unknown function '--lang'\n"
    );
}

#[test]
fn rrdcached_honors_posix_locale_and_rejects_non_rrdtool_lang_flag() {
    let dir = tempfile::tempdir().unwrap();
    let rrdcached = symlink_alias(dir.path(), "rrdcached");

    let en = clean_locale_cmd(&rrdcached)
        .env("LC_ALL", "C")
        .arg("-h")
        .output()
        .unwrap();
    assert_eq!(en.status.code(), Some(1));
    let en_out = String::from_utf8(en.stdout).unwrap();
    assert!(en_out.contains("RRDCacheD 1.11.0"));
    assert!(en_out.contains("Valid options are:"));

    let de = clean_locale_cmd(&rrdcached)
        .env("LC_ALL", "de_DE.UTF-8")
        .arg("-h")
        .output()
        .unwrap();
    assert_eq!(de.status.code(), Some(1));
    let de_out = String::from_utf8(de.stdout).unwrap();
    assert!(de_out.contains("RRDCacheD 1.11.0"));
    assert!(de_out.contains("-l <address>") || de_out.contains("-l "));
    assert_ne!(de_out, en_out);

    let no_lang = clean_locale_cmd(&rrdcached)
        .env("LC_ALL", "C")
        .args(["--lang", "de"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&no_lang.stderr).contains("invalid option -- 'lang'"));
}

#[test]
fn print_strftime_uses_pure_rust_lc_time_for_all_tier1_locales() {
    let dir = tempfile::tempdir().unwrap();
    let rrdtool = symlink_alias(dir.path(), "rrdtool");
    let rrd = dir.path().join("f.rrd");
    rondi::create_rrd_file(
        &rrd,
        1_000_000_000,
        60,
        &["DS:x:GAUGE:120:U:U".to_owned()],
        &["RRA:AVERAGE:0.5:1:10".to_owned()],
        true,
    )
    .unwrap();
    rondi::update_rrd_values(&rrd, 1_000_000_060, &[Some(10.0)]).unwrap();
    rondi::update_rrd_values(&rrd, 1_000_000_120, &[Some(20.0)]).unwrap();

    // BCP-47 tags (e.g. "ja", "ar", "hi", "zh-CN", "zh-TW") are not OS POSIX
    // locale names, so they reliably exercise Rondi's pure-Rust LC_TIME path
    // on both macOS and minimal Linux containers.
    let cases = [
        ("C", "Sunday September Sun AM"),
        ("ja", "日曜日 9月 日 午前"),
        ("ko", "일요일 9월 일 오전"),
        ("zh-CN", "星期日 九月 周日 上午"),
        ("zh-TW", "星期日 九月 週日 上午"),
        ("ar", "الأحد سبتمبر أحد ص"),
        ("hi", "रविवार सितंबर रवि पूर्वाह्न"),
        ("ru", "воскресенье сентябрь вс ДП"),
        ("uk", "неділя вересень нд дп"),
        ("tr", "Pazar Eylül Paz ÖÖ"),
        ("vi", "Chủ Nhật Tháng 9 CN SA"),
        ("id", "Minggu September Min AM"),
    ];
    let def = format!("DEF:x={}:x:AVERAGE", rrd.display());
    for (lc_time, expected) in cases {
        let out = clean_locale_cmd(&rrdtool)
            .env("LC_ALL", "")
            .env("LC_TIME", lc_time)
            .args([
                "graph",
                "/dev/null",
                "--start",
                "1000000000",
                "--end",
                "1000000300",
                &def,
                "VDEF:v=x,MAXIMUM",
                "PRINT:v:%A %B %a %p:strftime",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "lc_time={lc_time}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let line = stdout.lines().nth(1).unwrap_or("");
        assert_eq!(line, expected, "lc_time={lc_time}");
    }

    // Verify all 20 locales can render native `rondi --lang <code> --help`.
    for &locale in Locale::ALL {
        let out = clean_locale_cmd(rondi_bin())
            .args(["--lang", locale.code(), "--help"])
            .output()
            .unwrap();
        assert!(out.status.success(), "locale={locale}");
    }

    // Verify all 31 extended Kadupul LC_TIME locales render authentic localized
    // PRINT strftime output via pure-Rust tables even without OS locale packs.
    let kadupul_cases = [
        ("af-ZA", "Sondag September 2001-09-09"),
        ("am-ET", "እሑድ ሴፕቴምበር 09/09/2001"),
        ("ar-EG", "الأحد سبتمبر 09/09/2001"),
        ("ar-MA", "الأحد شتنبر 09/09/2001"),
        ("ar-SA", "الأحد سبتمبر 09/09/2001"),
        ("bg-BG", "неделя септември 09.09.2001"),
        ("bn-BD", "রবিবার সেপ্টেম্বর 09/09/2001"),
        ("el-GR", "Κυριακή Σεπτέμβριος 09/09/2001"),
        ("en-GB", "Sunday September 09/09/2001"),
        ("es-MX", "domingo septiembre 09/09/2001"),
        ("fa-IR", "یکشنبه سپتامبر 2001/09/09"),
        ("fr-CA", "dimanche septembre 2001-09-09"),
        ("fr-MA", "dimanche septembre 09/09/2001"),
        ("fr-SN", "dimanche septembre 09/09/2001"),
        ("ha-NG", "Lahadi Satumba 09/09/2001"),
        ("he-IL", "יום ראשון ספטמבר 09.09.2001"),
        ("ig-NG", "Sọnde Septemba 09/09/2001"),
        ("lv-LV", "svētdiena septembris 09.09.2001"),
        ("mg-MG", "Alahady Septambra 2001-09-09"),
        ("mr-IN", "रविवार सप्टेंबर 09/09/2001"),
        ("pt-PT", "domingo setembro 09/09/2001"),
        ("rw-RW", "Ku cyumweru Nzeli 2001-09-09"),
        ("so-SO", "Axad Sebteembar 09/09/2001"),
        ("sw-KE", "Jumapili Septemba 09/09/2001"),
        ("ta-IN", "ஞாயிறு செப்டம்பர் 09/09/2001"),
        ("te-IN", "ఆదివారం సెప్టెంబర్ 09-09-2001"),
        ("th-TH", "วันอาทิตย์ กันยายน 09/09/2001"),
        ("ur-PK", "اتوار ستمبر 09/09/2001"),
        ("xh-ZA", "Cawe Septemba 2001-09-09"),
        ("yo-NG", "Ọjọ́ Àìkú Owewe 09/09/2001"),
        ("zu-ZA", "ISonto Septhemba 2001-09-09"),
    ];
    for (lc_time, expected) in kadupul_cases {
        let out = clean_locale_cmd(&rrdtool)
            .env("LC_ALL", "")
            .env("LC_TIME", lc_time)
            .args([
                "graph",
                "/dev/null",
                "--start",
                "1000000000",
                "--end",
                "1000000300",
                &def,
                "VDEF:v=x,MAXIMUM",
                "PRINT:v:%A %B %x:strftime",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "lc_time={lc_time}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let line = stdout.lines().nth(1).unwrap_or("");
        assert_eq!(line, expected, "lc_time={lc_time}");
    }

    // Verify all 51 TimeLocale variants are covered and regional CLI tags map cleanly.
    assert_eq!(TimeLocale::ALL.len(), 51);
    for (tag, expected_cli) in [
        ("ar-MA", Locale::Ar),
        ("ar-EG", Locale::Ar),
        ("ar-SA", Locale::Ar),
        ("es-MX", Locale::Es),
        ("fr-CA", Locale::Fr),
        ("fr-MA", Locale::Fr),
        ("fr-SN", Locale::Fr),
        ("pt-PT", Locale::PtBr),
        ("en-GB", Locale::En),
    ] {
        assert_eq!(Locale::parse(tag), Some(expected_cli), "tag={tag}");
        let out = clean_locale_cmd(rondi_bin())
            .args(["--lang", tag, "--help"])
            .output()
            .unwrap();
        assert!(out.status.success(), "native --lang {tag}");
    }
}
