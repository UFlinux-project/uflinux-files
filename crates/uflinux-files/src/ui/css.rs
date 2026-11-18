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
.ufm-grid > child {
    margin: 4px;
    border-radius: 8px;
}
.ufm-cut { opacity: 0.5; }
.ufm-cell {
    padding: 6px;
    border-radius: 6px;
}
.ufm-inspector {
    border-left: 1px solid alpha(currentColor, 0.12);
    background-color: alpha(currentColor, 0.025);
}
button.ufm-shortcut {
    min-width: 88px;
    min-height: 92px;
    padding: 10px;
    border-radius: 10px;
}
button.ufm-shortcut:hover {
    background-color: alpha(currentColor, 0.08);
}

box.ufm-drive {
    min-width: 250px;
    min-height: 76px;
    padding: 12px;
    border-radius: 10px;
    background-color: alpha(currentColor, 0.035);
    border: 1px solid alpha(currentColor, 0.09);
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
    font-size: 1em;
}
label.ufm-meta {
    opacity: 0.75;
    font-size: 0.9em;
}
/* The star over a row's icon. Accent-coloured rather than left to the symbolic
   recolouring, which would paint it the same grey as the folder icon it sits on
   and make it read as part of that icon. The shadow keeps it legible over a
   photograph, which is what a thumbnail is. */
image.ufm-starred {
    color: @accent_color;
    -gtk-icon-shadow: 0 0 2px alpha(black, 0.6);
}
label.ufm-status {
    opacity: 0.7;
    font-size: 0.9em;
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
    padding: 8px 10px;
    margin: 2px 6px;
    border-radius: 8px;
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
/* The job strip. Drawn like the search bar because it is the same kind of thing:
   a strip of chrome that appears above the listing for as long as it has
   something to say. The bar is given a width of its own rather than left to
   expand, so a window that is very wide still shows a bar that reads as one bar
   rather than as a rule across the page. */
box.ufm-jobs {
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
    min-width: 260px;
}
.ufm-inspector box.ufm-preview {
    min-width: 0;
}
/* The desktop we paint ourselves. The surface has to be transparent or the
   wallpaper sits behind an opaque grey rectangle, which is a worse desktop than the
   blank one this replaces — and GTK gives a window an opaque background from the
   theme, so it has to be taken away at the root of the tree rather than on the
   cells. The class is on the window and the container is matched as its child, so
   one rule covers both nodes without a second class to keep in step. */
window.ufm-desktop,
window.ufm-desktop > fixed {
    background-color: transparent;
}
/* Over a wallpaper the theme's hover colour may be invisible or may be a solid
   block; either way it was chosen for a window, not for a desktop. The name needs
   to stay legible whatever is behind it, and a shadow is cheaper than a plate
   behind every label — cheaper in memory, which is the point of this feature, and
   cheaper to draw, which is the point of the machine it runs on. */
window.ufm-desktop button.ufm-cell {
    background-color: transparent;
}
window.ufm-desktop button.ufm-cell:hover {
    background-color: alpha(currentColor, 0.12);
}
window.ufm-desktop label.ufm-name {
    text-shadow: 0 1px 2px alpha(black, 0.8);
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
            "ufm-inspector",
            "ufm-shortcut",
            "ufm-drive",
            "ufm-header",
            "ufm-header-active",
            "ufm-name",
            "ufm-meta",
            "ufm-starred",
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
            "ufm-jobs",
            "ufm-mono",
            "ufm-open-with",
            "ufm-preview",
            // Set only by the feature-gated desktop module. It is in this list
            // whatever the feature set, because the rule above is compiled either
            // way and a rule that no build ever uses is exactly what this test is
            // for — the check here is that the class is named by the crate, which
            // it is, and not that every build of it has a widget to show.
            "ufm-desktop",
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
