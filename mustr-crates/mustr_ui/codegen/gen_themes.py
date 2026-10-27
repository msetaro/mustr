#!/usr/bin/env python3
"""Generates assets/themes/mustr/mustr.json from mustr's design tokens.

Usage: python3 mustr-crates/mustr_ui/codegen/gen_themes.py assets/themes/mustr/mustr.json

The tokens here must match `mustr_ui::tokens`; the crate's tests check the generated
JSON against them. Edit the tokens, regenerate, and commit both.
"""
import json
import sys


def parse(c):
    c = c.lstrip("#")
    return tuple(int(c[i : i + 2], 16) for i in (0, 2, 4))


def hx(rgb, a=1.0):
    r, g, b = rgb if isinstance(rgb, tuple) else parse(rgb)
    return "#%02x%02x%02x%02x" % (r, g, b, round(a * 255))


def mix(c1, c2, t):
    """Opaque mix: t is the fraction of c2."""
    a, b = parse(c1), parse(c2)
    return hx(tuple(round(x * (1 - t) + y * t) for x, y in zip(a, b)))


def alpha(c, a):
    return hx(parse(c), a)


TRANSPARENT = "#00000000"

DARK = dict(
    appearance="dark",
    bg="#0f1318",
    surface="#151a21",
    surface2="#1c232c",
    border="#29313b",
    text="#d6dce3",
    muted="#8792a0",
    accent="#5aa2ff",
    added=((46, 160, 67), 0.14, 0.42),
    deleted=((248, 81, 73), 0.12, 0.40),
    warning="#d29922",
    ok="#3fb950",
    pending="#b083f0",
    agent="#d97757",
    # Derived (not in the token table).
    error="#f85149",  # RGB of the deleted-line token
    elevated="#1c232c",  # Surface 2
    active_line_alpha=0.75,
    syntax=dict(
        keyword="#ff7b72",
        function="#d2a8ff",
        string="#a5d6ff",
        number="#79c0ff",
        type="#ffa657",
        property="#7ee787",
        comment="#8b949e",
    ),
    ansi=dict(
        black=("#484f58", "#6e7681"),
        red=("#ff7b72", "#ffa198"),
        green=("#3fb950", "#56d364"),
        yellow=("#d29922", "#e3b341"),
        blue=("#58a6ff", "#79c0ff"),
        magenta=("#bc8cff", "#d2a8ff"),
        cyan=("#39c5cf", "#56d4dd"),
        white=("#b1bac4", "#ffffff"),
    ),
    players=["#5aa2ff", "#d97757", "#b083f0", "#3fb950", "#d29922", "#39c5cf", "#f778ba", "#ff7b72"],
    selection_alpha=0.24,
    accents=["#5aa2ff", "#d2a8ff", "#7ee787", "#ffa657", "#ff7b72", "#79c0ff", "#d29922"],
)

LIGHT = dict(
    appearance="light",
    bg="#ffffff",
    surface="#f6f8fa",
    surface2="#eceff3",
    border="#d8dee4",
    text="#1f2328",
    muted="#636c76",
    accent="#0969da",
    added=("#e6ffec", None, "#abf2bc"),
    deleted=("#ffebe9", None, "#ffc1bd"),
    warning="#9a6700",
    ok="#1a7f37",
    pending="#8250df",
    agent="#d97757",
    error="#cf222e",
    elevated="#ffffff",  # Background: light popovers read as white cards with a border
    active_line_alpha=0.6,
    syntax=dict(
        keyword="#cf222e",
        function="#8250df",
        string="#0a3069",
        number="#0550ae",
        type="#953800",
        property="#116329",
        comment="#6e7781",
    ),
    ansi=dict(
        black=("#24292f", "#57606a"),
        red=("#cf222e", "#a40e26"),
        green=("#116329", "#1a7f37"),
        yellow=("#4d2d00", "#633c01"),
        blue=("#0969da", "#218bff"),
        magenta=("#8250df", "#a475f9"),
        cyan=("#1b7c83", "#3192aa"),
        white=("#6e7781", "#8c959f"),
    ),
    players=["#0969da", "#d97757", "#8250df", "#1a7f37", "#9a6700", "#1b7c83", "#bf3989", "#cf222e"],
    selection_alpha=0.2,
    accents=["#0969da", "#8250df", "#116329", "#953800", "#cf222e", "#0550ae", "#9a6700"],
)

COLORBLIND = dict(DARK)
COLORBLIND.update(
    added=((56, 139, 253), 0.15, 0.46),
    deleted=((219, 109, 40), 0.14, 0.46),
    ok="#58a6ff",
    # Derived: deleted/removed signals use the deleted token's orange, and
    # renamed moves to purple so it can't be confused with added (blue).
    deleted_solid="#db6d28",
    renamed="#b083f0",
    ansi=dict(
        DARK["ansi"],
        red=("#f0883e", "#ffa657"),
        green=("#58a6ff", "#79c0ff"),
    ),
)


def diff_colors(p, which):
    spec = p[which]
    if p["appearance"] == "dark":
        rgb, a, strong = spec
        return dict(
            background=hx(rgb, a),
            hollow_background=hx(rgb, a / 2),
            strong=hx(rgb, strong),
        )
    base, _, strong = spec
    return dict(
        background=hx(base),
        hollow_background=mix(base, p["bg"], 0.5),
        strong=hx(strong),
    )


def theme(name, p):
    bg, s1, s2, bd = p["bg"], p["surface"], p["surface2"], p["border"]
    tx, mu, ac = p["text"], p["muted"], p["accent"]
    dark = p["appearance"] == "dark"
    placeholder = mix(mu, bg, 0.3)
    line_number = mix(mu, bg, 0.25)
    element_hover = mix(s2, bd, 0.5)
    deleted_solid = p.get("deleted_solid", p["error"])
    renamed = p.get("renamed", ac)
    added = diff_colors(p, "added")
    deleted = diff_colors(p, "deleted")
    syn = p["syntax"]

    def o(c):
        return hx(c)

    style = {
        "background.appearance": "opaque",
        "border": o(bd),
        "border.variant": o(bd),
        "border.focused": o(ac),
        "border.selected": mix(ac, bg, 0.5),
        "border.transparent": TRANSPARENT,
        "border.disabled": o(s2),
        "elevated_surface.background": o(p["elevated"]),
        "surface.background": o(s1),
        "background": o(s1),
        "element.background": o(s2),
        "element.hover": element_hover,
        "element.active": o(bd),
        "element.selected": alpha(ac, 0.2),
        "element.disabled": o(s2),
        "element.selection_background": alpha(ac, 0.3),
        "drop_target.background": alpha(ac, 0.15),
        "drop_target.border": o(ac),
        "ghost_element.background": TRANSPARENT,
        "ghost_element.hover": alpha(mu, 0.12),
        "ghost_element.active": alpha(mu, 0.2),
        "ghost_element.selected": alpha(ac, 0.15),
        "ghost_element.disabled": TRANSPARENT,
        "text": o(tx),
        "text.muted": o(mu),
        "text.placeholder": placeholder,
        "text.disabled": placeholder,
        "text.accent": o(ac),
        "icon": o(tx),
        "icon.muted": o(mu),
        "icon.disabled": placeholder,
        "icon.placeholder": o(mu),
        "icon.accent": o(ac),
        "debugger.accent": o(p["error"]),
        "status_bar.background": o(s1),
        "title_bar.background": o(s1),
        "title_bar.inactive_background": o(bg),
        "toolbar.background": o(bg),
        "tab_bar.background": o(s1),
        "tab.inactive_background": o(s1),
        "tab.active_background": o(bg),
        "search.match_background": alpha(ac, 0.3),
        "search.active_match_background": alpha(p["warning"], 0.4),
        "panel.background": o(s1),
        "panel.focused_border": o(ac),
        "panel.indent_guide": o(bd),
        "panel.indent_guide_hover": o(mu),
        "panel.indent_guide_active": placeholder,
        "panel.overlay_background": o(s1),
        "panel.overlay_hover": element_hover,
        "pane.focused_border": o(ac),
        "pane_group.border": o(bd),
        "scrollbar.thumb.background": alpha(mu, 0.3),
        "scrollbar.thumb.hover_background": alpha(mu, 0.45),
        "scrollbar.thumb.active_background": alpha(mu, 0.6),
        "scrollbar.thumb.border": TRANSPARENT,
        "scrollbar.track.background": TRANSPARENT,
        "scrollbar.track.border": TRANSPARENT,
        "minimap.thumb.background": alpha(mu, 0.15),
        "minimap.thumb.hover_background": alpha(mu, 0.25),
        "minimap.thumb.active_background": alpha(mu, 0.35),
        "minimap.thumb.border": TRANSPARENT,
        "editor.foreground": o(tx),
        "editor.background": o(bg),
        "editor.gutter.background": o(bg),
        "editor.subheader.background": o(s1),
        "editor.active_line.background": alpha(s2, p["active_line_alpha"]),
        "editor.highlighted_line.background": o(s2),
        "editor.debugger_active_line.background": alpha(p["warning"], 0.15),
        "editor.line_number": line_number,
        "editor.active_line_number": o(tx),
        "editor.hover_line_number": o(mu),
        "editor.invisible": mix(bd, mu, 0.35),
        "editor.wrap_guide": alpha(tx, 0.05),
        "editor.active_wrap_guide": alpha(tx, 0.1),
        "editor.indent_guide": o(s2),
        "editor.indent_guide_active": o(bd),
        "editor.document_highlight.read_background": alpha(ac, 0.1),
        "editor.document_highlight.write_background": alpha(mu, 0.25),
        "editor.document_highlight.bracket_background": alpha(ac, 0.2),
        "editor.code_lens.foreground": o(mu),
        "editor.diff_hunk.added.background": added["background"],
        "editor.diff_hunk.added.hollow_background": added["hollow_background"],
        "editor.diff_hunk.added.hollow_border": added["strong"],
        "editor.diff_hunk.deleted.background": deleted["background"],
        "editor.diff_hunk.deleted.hollow_background": deleted["hollow_background"],
        "editor.diff_hunk.deleted.hollow_border": deleted["strong"],
        "terminal.background": o(bg),
        "terminal.foreground": o(tx),
        "terminal.bright_foreground": mix(tx, "#ffffff" if dark else "#000000", 0.5),
        "terminal.dim_foreground": o(mu),
        "terminal.ansi.background": o(bg),
    }
    for color, (normal, bright) in p["ansi"].items():
        style[f"terminal.ansi.{color}"] = o(normal)
        style[f"terminal.ansi.bright_{color}"] = o(bright)
        style[f"terminal.ansi.dim_{color}"] = mix(normal, bg, 0.35)
    style.update(
        {
            "link_text.hover": o(ac),
            "version_control.added": o(p["ok"]),
            "version_control.deleted": o(deleted_solid),
            "version_control.modified": o(p["warning"]),
            "version_control.renamed": o(renamed),
            "version_control.conflict": o(p["warning"]),
            "version_control.ignored": placeholder,
            "version_control.word_added": added["strong"],
            "version_control.word_deleted": deleted["strong"],
            "version_control.conflict_marker.ours": alpha(p["ok"], 0.1),
            "version_control.conflict_marker.theirs": alpha(ac, 0.1),
        }
    )
    status = {
        "conflict": p["warning"],
        "created": p["ok"],
        "deleted": deleted_solid,
        "error": p["error"],
        "hidden": placeholder,
        "hint": mu,
        "ignored": placeholder,
        "info": ac,
        "modified": p["warning"],
        "predictive": placeholder,
        "renamed": renamed,
        "success": p["ok"],
        "unreachable": mu,
        "warning": p["warning"],
    }
    for key, color in status.items():
        style[key] = o(color)
        style[f"{key}.background"] = alpha(color, 0.1)
        style[f"{key}.border"] = mix(color, bg, 0.6)

    style["players"] = [
        {"cursor": o(c), "background": o(c), "selection": alpha(c, p["selection_alpha"])} for c in p["players"]
    ]
    style["accents"] = [o(c) for c in p["accents"]]

    def c(color, **kw):
        d = {"color": o(color)}
        d.update(kw)
        return d

    style["syntax"] = {
        "attribute": c(syn["number"]),
        "boolean": c(syn["number"]),
        "comment": c(syn["comment"]),
        "comment.doc": c(syn["comment"]),
        "constant": c(syn["number"]),
        "constructor": c(syn["function"]),
        "diff.minus": c(deleted_solid),
        "diff.plus": c(p["ok"]),
        "embedded": c(tx),
        "emphasis": c(tx, font_style="italic"),
        "emphasis.strong": c(tx, font_weight=700),
        "enum": c(syn["type"]),
        "function": c(syn["function"]),
        "function.builtin": c(syn["function"]),
        "hint": c(mu),
        "keyword": c(syn["keyword"]),
        "label": c(syn["number"]),
        "link_text": c(syn["number"]),
        "link_uri": c(syn["string"]),
        "namespace": c(tx),
        "number": c(syn["number"]),
        "operator": c(syn["keyword"]),
        "predictive": c(placeholder, font_style="italic"),
        "preproc": c(syn["keyword"]),
        "primary": c(tx),
        "property": c(syn["property"]),
        "punctuation": c(tx),
        "punctuation.bracket": c(tx),
        "punctuation.delimiter": c(tx),
        "punctuation.list_marker": c(syn["type"]),
        "punctuation.markup": c(syn["keyword"]),
        "punctuation.special": c(syn["keyword"]),
        "selector": c(syn["property"]),
        "selector.pseudo": c(syn["function"]),
        "string": c(syn["string"]),
        "string.escape": c(syn["number"]),
        "string.regex": c(syn["property"]),
        "string.special": c(syn["number"]),
        "string.special.symbol": c(syn["number"]),
        "tag": c(syn["property"]),
        "text.literal": c(syn["string"]),
        "title": c(syn["number"], font_weight=700),
        "type": c(syn["type"]),
        "variable": c(tx),
        "variable.parameter": c(tx),
        "variable.special": c(syn["keyword"]),
        "variant": c(syn["number"]),
    }
    return {"name": name, "appearance": p["appearance"], "style": style}


family = {
    "$schema": "https://zed.dev/schema/themes/v0.2.0.json",
    "name": "mustr",
    "author": "The mustr authors",
    "themes": [
        theme("mustr Dark", DARK),
        theme("mustr Light", LIGHT),
        theme("mustr Colorblind-safe", COLORBLIND),
    ],
}

with open(sys.argv[1], "w") as f:
    json.dump(family, f, indent=2)
    f.write("\n")
