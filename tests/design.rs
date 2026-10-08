//! The design tokens in `ui/theme.slint` are *measured*, not trusted: this test
//! reads the file and checks WCAG contrast on the real surfaces, in both themes.

use std::collections::HashMap;

fn tokens() -> HashMap<String, (String, String)> {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/theme.slint")).unwrap();
    let mut out = HashMap::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("out property <color> ") else { continue };
        let Some((name, value)) = rest.split_once(':') else { continue };
        // Only plain 6-digit hex colours: `dark ? #AAAAAA : #BBBBBB;` or a single value.
        let hexes: Vec<String> = value
            .split('#')
            .skip(1)
            .filter_map(|p| {
                let h: String = p.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                (h.len() == 6).then_some(h)
            })
            .collect();
        match hexes.as_slice() {
            [a, b] => out.insert(name.trim().to_string(), (a.clone(), b.clone())),
            [a] => out.insert(name.trim().to_string(), (a.clone(), a.clone())),
            _ => None,
        };
    }
    out
}

fn lum(hex: &str) -> f64 {
    let c = |i: usize| {
        let v = u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * c(0) + 0.7152 * c(2) + 0.0722 * c(4)
}

fn contrast(a: &str, b: &str) -> f64 {
    let (x, y) = (lum(a), lum(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

const SURFACES: [&str; 4] = ["canvas", "sidebar", "surface", "elevated"];

fn pick(t: &HashMap<String, (String, String)>, name: &str, dark: bool) -> String {
    let v = t.get(name).unwrap_or_else(|| panic!("token {name} missing"));
    if dark { v.0.clone() } else { v.1.clone() }
}

#[test]
fn design_tokens_meet_contrast_targets() {
    let t = tokens();
    let mut report = vec![];
    for dark in [true, false] {
        let theme = if dark { "dark" } else { "light" };
        for surface in SURFACES {
            let bg = pick(&t, surface, dark);
            // Body text and anything that carries meaning: 4.5:1.
            for fg in ["text", "text-2", "text-3", "amber-text", "purple", "lavender", "success", "warning", "error"] {
                let c = contrast(&pick(&t, fg, dark), &bg);
                report.push(format!("{theme:5} {fg:11} on {surface:8} {c:5.2}"));
                assert!(c >= 4.5, "{theme}: {fg} on {surface} is {c:.2}:1, need 4.5:1");
            }
            // Boundaries of controls and the keyboard focus ring: 3:1.
            for fg in ["border-control", "focus"] {
                let c = contrast(&pick(&t, fg, dark), &bg);
                report.push(format!("{theme:5} {fg:11} on {surface:8} {c:5.2}"));
                assert!(c >= 3.0, "{theme}: {fg} on {surface} is {c:.2}:1, need 3:1");
            }
        }
        // The primary button: dark label on amber.
        let c = contrast(&pick(&t, "on-accent", dark), &pick(&t, "amber", dark));
        assert!(c >= 7.0, "{theme}: on-accent on amber is {c:.2}:1");
        // Amber progress fill against its track (graphical object): 3:1 on dark; text beside it carries the number anyway.
        if dark {
            let c = contrast(&pick(&t, "amber-deep", dark), &pick(&t, "elevated", dark));
            assert!(c >= 3.0, "progress fill on track is {c:.2}:1");
        }
    }
    // The decorative card outline is allowed to be faint: nothing depends on seeing it.
    eprintln!("{}", report.join("\n"));
}

#[test]
fn every_status_colour_is_paired_with_a_shape_in_the_components() {
    // Colour is never the only channel: the badge draws an icon and a word.
    let c = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/components.slint")).unwrap();
    assert!(c.contains("component Badge"));
    for shape in ["check", "alert", "x", "dot", "ring"] {
        assert!(c.contains(&format!("\"{shape}\"")), "badge shape {shape} missing");
    }
    assert!(c.contains("Icon { name: root.shape"));
}
