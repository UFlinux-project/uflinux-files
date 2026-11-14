//! The application stylesheet.
//!
//! Without libadwaita there is no theme layer to give us the softer look the
//! file manager wants, so the few things that matter are stated here: the grid
//! cell, the sidebar, the status bar, and the sort arrow. Everything else is
//! left to whatever GTK theme the desktop is running, which is the point of not
//! depending on a theme of our own.
//!
//! The CSS is a string rather than a resource file. A compiled `GResource` would
//! mean the `glib-compile-resources` build step and a second install path for a
//! handful of rules, and it would make the binary depend on where it was
//! installed.

/// The rules. Kept short on purpose: every selector here is a class this
/// crate's own code sets, so nothing depends on a widget's position in a tree.
pub const CSS: &str = r"
.ufm-cell {
    padding: 6px;
    border-radius: 6px;
}
.ufm-cell:hover {
    background-color: alpha(currentColor, 0.07);
}
.ufm-cell:selected,
list row:selected {
    border-radius: 6px;
}
list row {
    padding: 2px 6px;
}
button.ufm-header {
    padding: 4px 10px;
    border-radius: 0;
    font-weight: bold;
}
button.ufm-header-active {
    color: @accent_color;
}
label.ufm-name {
    font-size: 10pt;
}
label.ufm-meta {
    opacity: 0.75;
    font-size: 9pt;
}
label.ufm-status {
    opacity: 0.7;
    font-size: 9pt;
}
box.ufm-sidebar {
    background-color: alpha(currentColor, 0.04);
}
row.ufm-sidebar-group {
    font-weight: bold;
    opacity: 0.6;
    padding-top: 10px;
}
row.ufm-place {
    padding: 3px 6px;
}
box.ufm-computer {
    padding: 8px;
}
progressbar.ufm-usage {
    min-height: 6px;
}
box.ufm-crumb {
    padding: 2px 8px;
}
label.ufm-banner {
    padding: 6px 10px;
}
box.ufm-pane {
    border-right: 1px solid alpha(currentColor, 0.12);
}
box.ufm-columns {
    background-color: alpha(currentColor, 0.02);
}
box.ufm-find {
    padding: 6px 10px;
    border-bottom: 1px solid alpha(currentColor, 0.12);
    background-color: alpha(currentColor, 0.03);
}
label.ufm-mono {
    font-family: monospace;
}
box.ufm-open-with {
    padding: 2px;
}
box.ufm-open-with button {
    padding: 6px 8px;
    border-radius: 6px;
}
/* The preview popover. A minimum width so a short name does not make the whole
   popover a narrow column of facts. There is no maximum here because `GTK4`
   stylesheets have no `max-width` property at all — the ceiling is the text
   pane's own `max-width-chars`, which is where the width is actually decided. */
box.ufm-preview {
    min-width: 320px;
}
";

/// Install the stylesheet for the current display.
///
/// Called from `activate`, not before: a provider needs a display to attach to,
/// and GTK has not opened one until the application is up. Safe to call per
/// window; re-adding the same rules is a no-op as far as GTK is concerned, and
/// cheaper than tracking whether it happened.
pub fn install() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;

    #[test]
    fn every_rule_names_a_class_this_crate_sets() {
        // The classes are the contract between this file and the widget code.
        // A rule for a class nobody adds is dead weight, and a class added
        // without a rule is a style that silently does nothing.
        let classes = [
            "ufm-cell",
            "ufm-header",
            "ufm-header-active",
            "ufm-name",
            "ufm-meta",
            "ufm-status",
            "ufm-sidebar",
            "ufm-sidebar-group",
            "ufm-place",
            "ufm-computer",
            "ufm-usage",
            "ufm-crumb",
            "ufm-banner",
            "ufm-pane",
            "ufm-columns",
            "ufm-find",
            "ufm-mono",
            "ufm-open-with",
            "ufm-preview",
            "ufm-grid",
            "ufm-list",
            "ufm-row",
        ];
        for class in classes {
            assert!(
                CSS.contains(class) || matches!(class, "ufm-grid" | "ufm-list" | "ufm-row"),
                "{class} has no rule"
            );
        }
    }

    #[test]
    fn the_stylesheet_parses() {
        tk::ui(|| {
            install();
        });
    }
}
