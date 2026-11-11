//! Small presentational building blocks shared across views.

use gpui::{
    AnyView, App, Context, ElementId, FontWeight, Hsla, IntoElement, Render, SharedString,
    Stateful, Window, div, prelude::*, px, svg,
};

use crate::assets::icon;
use crate::theme;

pub struct TooltipText(pub SharedString);

impl Render for TooltipText {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::raised())
            .border_1()
            .border_color(theme::border_strong())
            .shadow_md()
            .text_xs()
            .text_color(theme::text())
            .child(self.0.clone())
    }
}

pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |_, cx| {
        let t = text.clone();
        cx.new(|_| TooltipText(t)).into()
    }
}

pub fn icon_el(name: &str, color: Hsla, size_px: f32) -> gpui::Svg {
    svg()
        .path(icon(name))
        .size(px(size_px))
        .flex_none()
        .text_color(color)
}

pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon_name: Option<&str>,
    primary: bool,
) -> Stateful<gpui::Div> {
    let mut b = div()
        .id(id)
        .h(px(30.0))
        .px_3()
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .border_1();
    if primary {
        b = b
            .bg(theme::accent())
            .border_color(theme::accent())
            .text_color(theme::on_accent())
            .font_weight(FontWeight::SEMIBOLD)
            .hover(|s| s.opacity(0.9));
    } else {
        b = b
            .bg(theme::raised())
            .border_color(theme::border())
            .text_color(theme::text())
            .hover(|s| s.bg(theme::border()).border_color(theme::border_strong()));
    }
    if let Some(i) = icon_name {
        b = b.child(icon_el(
            i,
            if primary {
                theme::on_accent()
            } else {
                theme::muted()
            },
            14.0,
        ));
    }
    b.child(label.into())
}

pub fn kbd(text: &'static str) -> impl IntoElement {
    div()
        .px_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(theme::border_strong())
        .text_xs()
        .text_color(theme::faint())
        .child(text)
}

/// A small on/off switch with a label.
pub fn switch(id: impl Into<ElementId>, label: &'static str, on: bool) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .px_2()
        .h(px(28.))
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .text_color(if on {
            theme::text_soft()
        } else {
            theme::muted()
        })
        .hover(|s| s.bg(theme::hover()))
        .child(
            div()
                .w(px(26.))
                .h(px(15.))
                .rounded_full()
                .p(px(2.))
                .flex()
                .when(on, |d| d.justify_end())
                .bg(if on {
                    theme::accent()
                } else {
                    theme::border_strong()
                })
                .child(div().size(px(11.)).rounded_full().bg(if on {
                    theme::on_accent()
                } else {
                    theme::muted()
                })),
        )
        .child(label)
}

/// A vertical divider for toolbars.
pub fn vsep() -> impl IntoElement {
    div().w(px(1.)).h(px(18.)).mx_1().bg(theme::border())
}
