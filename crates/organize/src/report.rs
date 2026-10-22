//! A plan as a page to review in a browser: what albums would be made from which folders, what
//! may be one trip, which files are copies of others. Self-contained HTML, no scripts.

use std::fmt::Write;

use crate::plan::{Plan, Sameness};

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Bytes as GB / MB.
fn size(b: u64) -> String {
    let g = b as f64 / 1_073_741_824.0;
    if g >= 1024.0 {
        format!("{:.2} TB", g / 1024.0)
    } else if g >= 1.0 {
        format!("{g:.1} GB")
    } else {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    }
}

fn dates(start: Option<&str>, end: Option<&str>) -> String {
    match (start.and_then(|s| s.get(..10)), end.and_then(|s| s.get(..10))) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a} – {b}"),
        _ => "no dates".into(),
    }
}

const STYLE: &str = "
:root { --bg: #fff; --fg: #1c1c1e; --dim: #6e6e73; --line: #e5e5ea; --accent: #0a64d6; --warn: #b25000; }
@media (prefers-color-scheme: dark) { :root { --bg: #1c1c1e; --fg: #f2f2f7; --dim: #a1a1a6; --line: #38383a; --accent: #5aa0ff; --warn: #ff9f43; } }
body { background: var(--bg); color: var(--fg); font: 14px/1.45 -apple-system, system-ui, sans-serif; margin: 0 auto; max-width: 1100px; padding: 24px 16px; }
h1 { font-size: 22px; } h2 { font-size: 17px; margin-top: 32px; border-bottom: 1px solid var(--line); padding-bottom: 4px; }
table { border-collapse: collapse; width: 100%; } td, th { text-align: left; padding: 5px 8px; border-bottom: 1px solid var(--line); vertical-align: top; }
th { color: var(--dim); font-weight: 600; font-size: 12px; } .n { text-align: right; white-space: nowrap; }
.dim { color: var(--dim); } .warn { color: var(--warn); } code { font-size: 12px; word-break: break-all; }
.stats { display: grid; grid-template-columns: repeat(auto-fill, minmax(170px, 1fr)); gap: 8px; }
.stat { border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px; } .stat b { display: block; font-size: 18px; }
details summary { cursor: pointer; }
";

/// The plan as an HTML page.
pub fn html(plan: &Plan) -> String {
    let s = &plan.stats;
    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Album plan</title><style>{STYLE}</style></head><body><h1>Album plan</h1>\
         <p class=\"dim\">From {}. Nothing has been changed: this is what LightCraft would make. To merge or rename albums or keep a copy, \
         write them in <code>edits.json</code> next to this page (or pass them to <code>cloud.planAlbums</code>) and make the plan again.</p><div class=\"stats\">",
        plan.roots.iter().map(|r| format!("<b>{}</b>", esc(r))).collect::<Vec<_>>().join(" and ")
    );
    for (label, value) in [
        ("albums", plan.albums.len().to_string()),
        ("photos", s.photos.to_string()),
        ("videos", s.videos.to_string()),
        ("scanned", format!("{} files, {}", s.files, size(s.bytes))),
        ("identical copies left out", format!("{} ({})", s.duplicate_copies, size(s.duplicate_bytes))),
        ("likely duplicates to review", s.likely_duplicates.to_string()),
        ("raw + JPEG pairs", s.raw_jpeg_pairs.to_string()),
        ("photos without a date", s.undated.to_string()),
        ("unreadable", s.unreadable.to_string()),
    ] {
        let _ = write!(h, "<div class=\"stat\"><b>{}</b>{}</div>", esc(&value), esc(label));
    }
    h.push_str("</div>");

    if !plan.suggestions.is_empty() {
        let _ = write!(h, "<h2>May be one trip ({})</h2><table><tr><th>Albums</th><th>Why</th></tr>", plan.suggestions.len());
        for sg in &plan.suggestions {
            let name = |k: &str| plan.albums.iter().find(|a| a.key == k).map_or_else(|| k.to_string(), |a| a.name.clone());
            let _ = write!(
                h,
                "<tr><td>{}<br>{}<br><span class=\"dim\">merge: <code>[\"{}\", \"{}\"]</code></span></td><td>{}</td></tr>",
                esc(&name(&sg.merge[0])),
                esc(&name(&sg.merge[1])),
                esc(&sg.merge[0]),
                esc(&sg.merge[1]),
                esc(&sg.reason)
            );
        }
        h.push_str("</table>");
    }

    let _ = write!(
        h,
        "<h2>Albums ({})</h2><table><tr><th>Album</th><th>Dates</th><th class=\"n\">Photos</th><th class=\"n\">Videos</th><th class=\"n\">Size</th><th>From</th></tr>",
        plan.albums.len()
    );
    for a in &plan.albums {
        let _ = write!(
            h,
            "<tr><td><b>{}</b><br><span class=\"dim\">key <code>{}</code></span></td><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td>{}</td></tr>",
            esc(&a.name),
            esc(&a.key),
            esc(&dates(a.start.as_deref(), a.end.as_deref())),
            a.photos,
            a.videos,
            esc(&size(a.bytes)),
            a.sources.iter().map(|x| esc(x)).collect::<Vec<_>>().join("<br>")
        );
    }
    h.push_str("</table>");

    let likely: Vec<_> = plan.duplicates.iter().filter(|d| d.sameness == Sameness::LikelySameShot).collect();
    if !likely.is_empty() {
        let _ = write!(
            h,
            "<h2>Likely duplicates ({})</h2><p class=\"dim\">The same shot (capture time to the millisecond, camera, size) in files that \
             differ: an edited or re-saved copy. Every copy is in the plan; list the ones to leave out after reviewing.</p><table>\
             <tr><th>Biggest copy</th><th>Others</th></tr>",
            likely.len()
        );
        for d in likely {
            let _ = write!(
                h,
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                esc(&d.keep.full()),
                d.others.iter().map(|o| format!("<code>{}</code>", esc(&o.full()))).collect::<Vec<_>>().join("<br>")
            );
        }
        h.push_str("</table>");
    }

    let same: Vec<_> = plan.duplicates.iter().filter(|d| d.sameness != Sameness::LikelySameShot).collect();
    if !same.is_empty() {
        let _ = write!(
            h,
            "<h2>Identical copies ({} left out)</h2><details><summary>Show them (the first file of each row is the one in the plan)</summary><table>",
            same.iter().map(|d| d.others.len()).sum::<usize>()
        );
        for d in same {
            let how =
                if d.sameness == Sameness::SameStart { " <span class=\"warn\">(same size, start and name; no hash to compare)</span>" } else { "" };
            let _ = write!(
                h,
                "<tr><td><code>{}</code>{how}</td><td>{}</td><td class=\"n\">{}</td></tr>",
                esc(&d.keep.full()),
                d.others.iter().map(|o| format!("<code>{}</code>", esc(&o.full()))).collect::<Vec<_>>().join("<br>"),
                esc(&size(d.size))
            );
        }
        h.push_str("</table></details>");
    }

    if !plan.unreadable.is_empty() {
        let _ = write!(
            h,
            "<h2 class=\"warn\">Unreadable ({})</h2><p class=\"dim\">In their folder's album, not checked for duplicates. A new scan tries them again.</p><table>",
            plan.unreadable.len()
        );
        for (f, e) in &plan.unreadable {
            let _ = write!(h, "<tr><td><code>{}</code></td><td>{}</td></tr>", esc(&f.full()), esc(e));
        }
        h.push_str("</table>");
    }
    h.push_str("</body></html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_escaped() {
        assert_eq!(esc("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
        assert_eq!(size(1_610_612_736), "1.5 GB");
        assert_eq!(dates(Some("2019-07-03T10:00:00"), Some("2019-07-03T18:00:00")), "2019-07-03");
    }
}
