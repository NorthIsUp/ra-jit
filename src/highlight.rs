//! Syntax highlighting for the editor, using rust-analyzer's own semantic highlighter (already a dependency).
//! Single-file analysis: no std, so std items are unresolved, but keywords, literals, fns, types, macros and
//! locals are classified. The egui LayoutJob is cached by text hash.

use std::hash::{DefaultHasher, Hash, Hasher};


use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId, Stroke};

use crate::theme::Theme;
use ra_ap_ide::{Analysis, Highlight, HighlightConfig, HlMod, HlPunct, HlTag};
use ra_ap_ide::{RaFixtureConfig, SymbolKind};
use ra_ap_paths::{AbsPathBuf, Utf8PathBuf};

/// The single mapping from rust-analyzer tags/modifiers to theme colours.
pub fn role_colour(t: &Theme, h: Highlight) -> Color32 {
    if h.mods.contains(HlMod::Documentation) {
        return t.doc_comment;
    }
    if h.mods.contains(HlMod::Unsafe) && !matches!(h.tag, HlTag::Comment | HlTag::StringLiteral) {
        return t.unsafe_item;
    }
    match h.tag {
        HlTag::Comment => t.comment,
        HlTag::Keyword | HlTag::BuiltinType | HlTag::BoolLiteral => t.keyword,
        HlTag::NumericLiteral => t.number,
        HlTag::StringLiteral | HlTag::CharLiteral | HlTag::ByteLiteral => t.string,
        HlTag::EscapeSequence | HlTag::FormatSpecifier | HlTag::InvalidEscapeSequence => t.keyword,
        HlTag::AttributeBracket => t.attribute,
        HlTag::Punctuation(HlPunct::MacroBang) => t.macro_call,
        HlTag::Operator(_) | HlTag::Punctuation(_) | HlTag::UnresolvedReference | HlTag::None => t.fg,
        HlTag::Symbol(k) => match k {
            SymbolKind::Function | SymbolKind::Method => t.function,
            SymbolKind::TypeParam | SymbolKind::LifetimeParam | SymbolKind::Label => t.type_param,
            SymbolKind::Const | SymbolKind::Static | SymbolKind::ConstParam | SymbolKind::Field | SymbolKind::Variant => t.constant,
            SymbolKind::Macro | SymbolKind::ProcMacro | SymbolKind::Derive | SymbolKind::DeriveHelper => t.macro_call,
            SymbolKind::Attribute | SymbolKind::BuiltinAttr | SymbolKind::ToolModule => t.attribute,
            SymbolKind::SelfParam => t.keyword,
            _ => t.fg, // structs, enums, traits, modules, locals, params: DEFAULT_CLASS_REFERENCE / DEFAULT_IDENTIFIER
        },
    }
}

fn style(t: &Theme, h: Highlight) -> (Color32, bool, bool) {
    let underline = h.mods.contains(HlMod::Mutable); // DEFAULT_REASSIGNED_LOCAL_VARIABLE: underline
    let italic = h.mods.contains(HlMod::Documentation) || matches!(h.tag, HlTag::Symbol(SymbolKind::Const | SymbolKind::Static));
    (role_colour(t, h), underline, italic)
}

fn config() -> HighlightConfig<'static> {
    HighlightConfig {
        strings: true,
        comments: true,
        punctuation: true,
        specialize_punctuation: true,
        operator: true,
        specialize_operator: true,
        inject_doc_comment: false,
        macro_bang: true,
        syntactic_name_ref_highlighting: true,
        ra_fixture: RaFixtureConfig::default(),
    }
}

/// Single-file analysis has no std, so `for x in 0..n` bindings are unresolved and the syntactic fallback guesses
/// `Const` for their uses. Real consts are UPPER_CASE; a name with lowercase letters is really a local.
fn demote_guess(mut h: Highlight, text: &str) -> Highlight {
    if matches!(h.tag, HlTag::Symbol(SymbolKind::Const | SymbolKind::Static)) && text.chars().any(|c| c.is_lowercase()) {
        h.tag = HlTag::Symbol(SymbolKind::Local);
    }
    h
}

fn build(t: &Theme, text: &str, font: &FontId) -> LayoutJob {
    let fmt = |(c, u, i): (Color32, bool, bool)| TextFormat {
        font_id: font.clone(),
        color: c,
        italics: i,
        line_height: Some(t.metrics.line_height),
        underline: if u { Stroke::new(t.metrics.underline_width, c) } else { Stroke::NONE },
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    let cwd = triomphe::Arc::new(AbsPathBuf::assert(Utf8PathBuf::from("/")));
    let (analysis, file) = Analysis::from_single_file(text.to_owned(), cwd);
    let ranges = analysis.highlight(config(), file).unwrap_or_default();
    let mut pos = 0usize;
    for r in ranges {
        let (s, e) = (usize::from(r.range.start()), usize::from(r.range.end()));
        if s < pos || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        if s > pos {
            job.append(&text[pos..s], 0.0, fmt((t.fg, false, false)));
        }
        job.append(&text[s..e], 0.0, fmt(style(t, demote_guess(r.highlight, &text[s..e]))));
        pos = e;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, fmt((t.fg, false, false)));
    }
    job
}

/// Synchronous, cached by (text, font) hash: the frame that shows new text always shows its colours.
#[derive(Default)]
pub struct Highlighter {
    key: Option<u64>,
    job: LayoutJob,
}

impl Highlighter {
    pub fn job(&mut self, t: &Theme, text: &str, font: FontId, wrap_width: f32) -> LayoutJob {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        font.hash(&mut h);
        let key = h.finish();
        if self.key != Some(key) {
            let started = std::time::Instant::now();
            self.job = build(t, text, &font);
            self.key = Some(key);
            if std::env::var_os("RAJIT_HL_TIMING").is_some() {
                eprintln!("highlight {} bytes: {:.2} ms", text.len(), started.elapsed().as_secs_f64() * 1e3);
            }
        }
        let mut job = self.job.clone();
        job.wrap.max_width = wrap_width;
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_const_guess_is_a_local() {
        let h = Highlight { tag: HlTag::Symbol(SymbolKind::Const), mods: Default::default() };
        assert_eq!(demote_guess(h, "y").tag, HlTag::Symbol(SymbolKind::Local));
        assert_eq!(demote_guess(h, "WIDTH").tag, HlTag::Symbol(SymbolKind::Const));
    }
}
