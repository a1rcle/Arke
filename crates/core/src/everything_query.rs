//! Translates Arke's search box syntax ([`crate::index_query::Query`]) into an Everything search
//! string.
//!
//! Pure computation with no OS or SDK calls, so all edge cases can be unit-tested on any machine.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::index::BUILD_DIRS;
use crate::index_query::{Clock, Kind, Query, TypeFilter};

/// Scope and exclusion options applied when building an Everything query.
#[derive(Clone, Debug, Default)]
pub struct EverythingQueryOptions<'a> {
    /// Restrict results to descendants of this folder (used for "Subfolders" scope).
    pub root: Option<&'a Path>,
    /// Skip named build output and package cache directories (`node_modules`, `.git`, ...).
    pub skip_build_dirs: bool,
    /// Local drives or folders excluded in settings.
    pub excluded_paths: &'a [PathBuf],
}

/// Output of translating an Arke [`Query`] into an Everything query string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslatedQuery {
    /// Search expression passed to `Everything_SetSearchW`.
    pub text: String,
    /// `true` when the user typed filters only (no name text), so results should be ordered newest
    /// first (`EVERYTHING_SORT_DATE_MODIFIED_DESCENDING`).
    pub prefer_newest: bool,
}

/// Extensions corresponding to each [`TypeFilter`] (kept in lockstep with
/// [`crate::filetype::kind_for_extension`]).
const fn type_extensions(filter: TypeFilter) -> Option<&'static str> {
    match filter {
        TypeFilter::Folder => None,
        TypeFilter::Document => Some(
            "pdf;doc;docx;odt;pages;xls;xlsx;xlsm;ods;numbers;ppt;pptx;odp;key;txt;log;md;markdown;rst;csv;tsv;rtf;nfo",
        ),
        TypeFilter::Image => Some(
            "png;jpg;jpeg;jfif;gif;bmp;webp;tif;tiff;ico;svg;heic;heif;avif;raw;cr2;nef;arw;dng;psd",
        ),
        TypeFilter::Audio => Some("mp3;wav;flac;aac;ogg;m4a;wma;opus;aiff"),
        TypeFilter::Video => Some("mp4;mkv;mov;avi;wmv;webm;flv;m4v;mpg;mpeg"),
        TypeFilter::Archive => Some("zip;7z;rar;tar;gz;tgz;bz2;xz;zst;cab;iso;img;vhd;vhdx;dmg"),
        TypeFilter::Code => Some(
            "rs;c;h;cpp;hpp;cc;cs;java;kt;go;py;rb;js;mjs;cjs;ts;tsx;jsx;vue;svelte;html;htm;css;scss;less;json;jsonc;toml;yaml;yml;xml;sql;sh;bash;ps1;psm1;bat;cmd;lua;php;swift;dart;zig;proto;graphql;ini;cfg;conf;lock",
        ),
        TypeFilter::Executable => Some("exe;msi;dll;sys;com;appx;msix"),
    }
}

/// Translates a parsed Arke [`Query`] into an Everything query string.
///
/// Returns `None` when the query has neither name text nor filters.
pub fn translate_query(
    query: &Query,
    clock: Clock,
    options: &EverythingQueryOptions<'_>,
) -> Option<TranslatedQuery> {
    if query.text.trim().is_empty() && !query.has_filters() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();

    if let Some(root) = options.root {
        let dir = ensure_trailing_backslash(&root.to_string_lossy());
        parts.push(format!("path:\"{}\"", escape_quotes(&dir)));
    }

    match query.kind {
        Some(Kind::File) => parts.push(String::from("file:")),
        Some(Kind::Folder) => parts.push(String::from("folder:")),
        None => {}
    }

    if !query.ext.is_empty() {
        parts.push(format!("ext:{}", query.ext.join(";")));
    }

    if !query.types.is_empty() {
        let clauses: Vec<String> = query
            .types
            .iter()
            .map(|&t| match type_extensions(t) {
                Some(exts) => format!("ext:{exts}"),
                None => String::from("folder:"),
            })
            .collect();
        if clauses.len() == 1 {
            if let Some(only) = clauses.into_iter().next() {
                parts.push(only);
            }
        } else {
            parts.push(format!("<{}>", clauses.join("|")));
        }
    }

    if let Some((lo, hi)) = query.size {
        if lo == 0 && hi == 0 {
            parts.push(String::from("size:0"));
        } else if hi == u64::MAX {
            parts.push(format!("size:>={lo}"));
        } else if lo == 0 {
            parts.push(format!("size:<={hi}"));
        } else {
            parts.push(format!("size:{lo}..{hi}"));
        }
    }

    if let Some((lo, hi)) = query.modified
        && let Some(clause) = translate_date_range(lo, hi, clock.utc_offset)
    {
        parts.push(clause);
    }

    if options.skip_build_dirs {
        for dir in BUILD_DIRS {
            parts.push(format!("!\\{dir}\\"));
        }
    }

    for excluded in options.excluded_paths {
        let raw = excluded.to_string_lossy();
        if !raw.trim().is_empty() {
            let dir = ensure_trailing_backslash(&raw);
            parts.push(format!("!path:\"{}\"", escape_quotes(&dir)));
        }
    }

    let cleaned_name = clean_name_text(&query.text);
    let prefer_newest = cleaned_name.is_empty();
    if !cleaned_name.is_empty() {
        parts.push(cleaned_name);
    }

    Some(TranslatedQuery {
        text: parts.join(" "),
        prefer_newest,
    })
}

/// Strips Arke's leading exact-match marker (`'word`) from unquoted tokens while preserving quoted
/// phrases and path separators (`docs\report`).
fn clean_name_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_quotes = false;
    let mut at_token_start = true;
    for ch in text.trim().chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                at_token_start = false;
                out.push(ch);
            }
            c if c.is_whitespace() && !in_quotes => {
                at_token_start = true;
                out.push(c);
            }
            '\'' if at_token_start && !in_quotes => {
                at_token_start = false;
            }
            _ => {
                at_token_start = false;
                out.push(ch);
            }
        }
    }
    out.trim().to_string()
}

fn ensure_trailing_backslash(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    format!("{trimmed}\\")
}

fn escape_quotes(s: &str) -> String {
    s.replace('"', "")
}

/// Converts a `[lo, hi)` Unix timestamp range to an ISO-8601 Everything `dm:` clause in local time.
///
/// Because Everything's date range is day-granular, this produces a conservative superset; callers
/// should also validate exact timestamps via [`Query::accepts`].
fn translate_date_range(lo: i64, hi: i64, utc_offset: i64) -> Option<String> {
    let has_lo = lo > i64::MIN / 2;
    let has_hi = hi < i64::MAX / 2;
    match (has_lo, has_hi) {
        (true, true) => {
            let start = format_local_date(lo.saturating_add(utc_offset));
            // `hi` is exclusive; subtract 1 second so `[today 00:00, tomorrow 00:00)` stays on today.
            let end = format_local_date(hi.saturating_sub(1).saturating_add(utc_offset));
            if start == end {
                Some(format!("dm:{start}"))
            } else {
                Some(format!("dm:{start}..{end}"))
            }
        }
        (true, false) => {
            let start = format_local_date(lo.saturating_add(utc_offset));
            Some(format!("dm:>={start}"))
        }
        (false, true) => {
            let end = format_local_date(hi.saturating_sub(1).saturating_add(utc_offset));
            Some(format!("dm:<={end}"))
        }
        (false, false) => None,
    }
}

/// Formats local Unix seconds as `YYYY-MM-DD` (Howard Hinnant's civil-from-days algorithm).
fn format_local_date(local_secs: i64) -> String {
    let days = local_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let mut out = String::with_capacity(10);
    let _ = write!(out, "{year:04}-{m:02}-{d:02}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC8: Clock = Clock {
        // 2026-11-26 12:00:00 UTC+8 (2026-11-26 04:00:00 UTC)
        now: 1_795_665_600,
        utc_offset: 8 * 3600,
    };

    #[test]
    fn empty_query_returns_none() {
        let q = Query::parse("   ", UTC8);
        assert!(translate_query(&q, UTC8, &EverythingQueryOptions::default()).is_none());
    }

    #[test]
    fn strips_exact_marker_and_keeps_path_terms() {
        let q = Query::parse(r"'report docs\draft", UTC8);
        let translated = translate_query(&q, UTC8, &EverythingQueryOptions::default()).unwrap();
        assert_eq!(translated.text, r"report docs\draft");
        assert!(!translated.prefer_newest);
    }

    #[test]
    fn subfolder_root_and_filters_translate_cleanly() {
        let q = Query::parse("ext:pdf;docx size:>10mb report", UTC8);
        let root = PathBuf::from(r"D:\Work\Specs");
        let opts = EverythingQueryOptions {
            root: Some(&root),
            skip_build_dirs: false,
            excluded_paths: &[],
        };
        let translated = translate_query(&q, UTC8, &opts).unwrap();
        assert_eq!(
            translated.text,
            r#"path:"D:\Work\Specs\" file: ext:pdf;docx size:>=10485761 report"#
        );
        assert!(!translated.prefer_newest);
    }

    #[test]
    fn filter_only_prefers_newest_and_maps_type_and_date() {
        let q = Query::parse("type:image dm:2026-11-26", UTC8);
        let translated = translate_query(&q, UTC8, &EverythingQueryOptions::default()).unwrap();
        assert!(translated.prefer_newest);
        assert!(translated.text.contains("ext:png;jpg;jpeg;"));
        assert!(translated.text.contains("dm:2026-11-26"));
    }

    #[test]
    fn multi_type_uses_or_group() {
        let q = Query::parse("type:folder;audio", UTC8);
        let translated = translate_query(&q, UTC8, &EverythingQueryOptions::default()).unwrap();
        assert!(translated.text.starts_with("<folder:|ext:mp3;wav;"));
    }

    #[test]
    fn exclusions_and_build_dirs_are_appended() {
        let q = Query::parse("Cargo.toml", UTC8);
        let excluded = vec![PathBuf::from(r"C:\Temp")];
        let opts = EverythingQueryOptions {
            root: None,
            skip_build_dirs: true,
            excluded_paths: &excluded,
        };
        let translated = translate_query(&q, UTC8, &opts).unwrap();
        assert!(translated.text.contains(r"!\node_modules\"));
        assert!(translated.text.contains(r"!\.git\"));
        assert!(translated.text.contains(r#"!path:"C:\Temp\""#));
        assert!(translated.text.ends_with("Cargo.toml"));
    }
}
