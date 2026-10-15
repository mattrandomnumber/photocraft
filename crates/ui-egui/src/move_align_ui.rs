//! The Move tool's align controls in the options bar, as Photoshop shows them whenever the Move
//! tool is active: Align Left Edges, Horizontal Centers and Right Edges, then Top Edges, Vertical
//! Centers and Bottom Edges, then `…` for the rest (Distribute and Align To). Every button runs a
//! `layer.align.*` / `layer.distribute.*` command, greyed out where the engine says it can't run.

use craft_ui::buttons::{ButtonVisuals, IconButton, IconButtonStyle};
use egui::{Color32, Vec2};
use serde_json::json;

use crate::PhotocraftApp;
use crate::theme::Tokens;

/// The six align buttons, in Photoshop's order; a gap goes before the vertical ones.
const ALIGN: [&str; 6] = ["leftEdges", "horizontalCenters", "rightEdges", "topEdges", "verticalCenters", "bottomEdges"];

/// The `to` param for the align commands from the Align To choice: Selection is the engine's
/// `auto` (the layers' own bounds, or the selection / canvas when one layer is selected).
fn align_to(app: &PhotocraftApp) -> serde_json::Value {
    if app.ui.tool_options.move_align_to == "canvas" { json!({"to": "canvas"}) } else { json!({}) }
}

/// Draws the align buttons and the `…` menu at the cursor, and runs the one clicked.
pub fn options(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let mut run: Option<(String, serde_json::Value)> = None;
    crate::widgets::vline(ui, 22.0);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, kind) in ALIGN.into_iter().enumerate() {
            if i == 3 {
                ui.add_space(6.0);
            }
            let id = format!("layer.align.{kind}");
            let on = app.session.is_enabled(&id);
            let label = photocraft_engine::commands::find(&id).map_or(kind, |c| c.label);
            let tip = crate::i18n::fmt(tl!("Align {what}"), &[("what", tl!(&label))]);
            if ui.add_enabled_ui(on, |ui| glyph_button(ui, kind, &tip, &t)).inner.clicked() {
                run = Some((id, align_to(app)));
            }
        }
    });
    ui.add_space(4.0);
    let more = crate::icons::button(ui, "ellipsis", 24.0, false, tl!("Align and distribute"));
    egui::Popup::menu(&more).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.set_min_width(210.0);
        let heading = |ui: &mut egui::Ui, text: &str| ui.label(egui::RichText::new(text).small().color(t.text_dim));
        heading(ui, tl!("Align"));
        for kind in ALIGN {
            let id = format!("layer.align.{kind}");
            let label = photocraft_engine::commands::find(&id).map_or(kind, |c| c.label);
            if ui.add_enabled(app.session.is_enabled(&id), egui::Button::new(tl!(label))).clicked() {
                run = Some((id, align_to(app)));
                ui.close();
            }
        }
        ui.separator();
        heading(ui, tl!("Distribute"));
        for c in photocraft_engine::command_specs().iter().filter(|c| c.id.starts_with("layer.distribute.")) {
            if ui.add_enabled(app.session.is_enabled(c.id), egui::Button::new(tl!(c.label))).clicked() {
                run = Some((c.id.to_string(), json!({})));
                ui.close();
            }
        }
        ui.separator();
        heading(ui, tl!("Align To"));
        let to = &mut app.ui.tool_options.move_align_to;
        ui.radio_value(to, "selection".to_string(), tl!("Selection"));
        ui.radio_value(to, "canvas".to_string(), tl!("Canvas"));
    });
    if let Some((id, params)) = run
        && let Err(e) = app.run(&id, params)
    {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

/// A square options-bar button showing the align glyph for `kind`.
fn glyph_button(ui: &mut egui::Ui, kind: &str, tip: &str, t: &Tokens) -> egui::Response {
    let style = IconButtonStyle {
        normal: ButtonVisuals { fill: Color32::TRANSPARENT, stroke: egui::Stroke::NONE, icon: if ui.is_enabled() { t.icon } else { t.text_faint } },
        hovered: ButtonVisuals { fill: t.hover, stroke: egui::Stroke::NONE, icon: t.text },
        pressed: None,
        selected: ButtonVisuals { fill: t.accent_soft, stroke: egui::Stroke::new(1.0, t.accent_border), icon: t.accent_text },
        corner_radius: t.radius_sm.into(),
        focus_stroke: egui::Stroke::new(1.0, t.accent),
    };
    IconButton::new(tip, Vec2::splat(24.0), &style)
        .show(ui, |ui, rect, tint| crate::layer_props_ui::align_glyph(ui, rect, kind, tint))
        .on_hover_text(tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    /// A 200 × 100 document whose active layer holds a 20 px square at (10, 10), with the
    /// align controls drawn in `theme`.
    fn harness(theme: crate::theme::ThemeKind) -> egui_kittest::Harness<'static, PhotocraftApp> {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 200, "height": 100})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("select.rect", json!({"x": 10, "y": 10, "width": 20, "height": 20})).unwrap();
        app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        app.sync_views();
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(700.0, 60.0)).build_ui_state(
            |ui, app: &mut PhotocraftApp| {
                ui.horizontal(|ui| options(app, ui));
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, theme);
        h.run_steps(2);
        h
    }

    #[test]
    fn every_theme_shows_photoshops_six_align_buttons() {
        for theme in crate::theme::ThemeKind::ALL {
            let h = harness(theme);
            for what in ["Left Edges", "Horizontal Centers", "Right Edges", "Top Edges", "Vertical Centers", "Bottom Edges"] {
                assert!(h.query_by_label(&format!("Align {what}")).is_some(), "{theme:?}: Align {what}");
            }
            assert!(h.query_by_label("Align and distribute").is_some(), "{theme:?}");
        }
    }

    #[test]
    fn clicking_an_align_button_aligns_the_layer() {
        let mut h = harness(crate::theme::ThemeKind::Studio);
        h.get_by_label("Align Right Edges").click();
        h.run_steps(2);
        assert!(!h.state().ui.status_error, "{}", h.state().ui.status);
        assert_eq!(right_edge(h.state_mut()), 200.0, "one layer, no selection: aligned to the canvas's right edge");
    }

    /// The active layer's right content edge.
    fn right_edge(app: &mut PhotocraftApp) -> f64 {
        let st = app.session.active().unwrap();
        let layer = st.doc.layer(st.active_layer.unwrap()).unwrap().clone();
        let s = layer.surface().unwrap();
        f64::from(app.cached_bounds(layer.id.0, s).x1)
    }

    #[test]
    fn align_to_canvas_passes_the_canvas_target() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        assert_eq!(align_to(&app), json!({}));
        app.ui.tool_options.move_align_to = "canvas".into();
        assert_eq!(align_to(&app), json!({"to": "canvas"}));
    }
}
