use std::{ops::Range, rc::Rc};

use gpui::{
    AnyElement, App, AppContext as _, AvailableSpace, Bounds, Element, ElementId, Entity,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Pixels, Render, ScrollHandle, StatefulInteractiveElement as _,
    StyleRefinement, Styled, Window, deferred, div, point, px,
};

use crate::{
    StyledExt,
    input::{HoverPopoverScroll, InputState, popovers::render_markdown},
};

pub struct HoverPopover {
    editor: Entity<InputState>,
    /// The symbol range byte of the hover trigger.
    pub(crate) symbol_range: Range<usize>,
    pub(crate) hover: Rc<lsp_types::Hover>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl HoverPopover {
    pub fn new(
        editor: Entity<InputState>,
        symbol_range: Range<usize>,
        hover: &lsp_types::Hover,
        cx: &mut App,
    ) -> Entity<Self> {
        let hover = Rc::new(hover.clone());

        cx.new(|cx| Self {
            editor,
            symbol_range,
            hover,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        })
    }

    pub(crate) fn is_same(&self, offset: usize) -> bool {
        self.symbol_range.contains(&offset)
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub(crate) fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub(crate) fn scroll(&self, command: HoverPopoverScroll) -> bool {
        scroll_hover_handle(&self.scroll_handle, command)
    }

    pub(crate) fn scroll_offsets(&self) -> (f32, f32) {
        (
            -self.scroll_handle.offset().y.as_f32(),
            self.scroll_handle.max_offset().y.as_f32(),
        )
    }
}

impl Render for HoverPopover {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let contents = match self.hover.contents.clone() {
            lsp_types::HoverContents::Scalar(scalar) => match scalar {
                lsp_types::MarkedString::String(s) => s,
                lsp_types::MarkedString::LanguageString(ls) => ls.value,
            },
            lsp_types::HoverContents::Array(arr) => arr
                .into_iter()
                .map(|item| match item {
                    lsp_types::MarkedString::String(s) => s,
                    lsp_types::MarkedString::LanguageString(ls) => ls.value,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            lsp_types::HoverContents::Markup(markup) => markup.value,
        };

        Popover::new(
            "hover-popover",
            self.editor.clone(),
            self.symbol_range.clone(),
            move |window, cx| render_markdown("message", contents.clone(), window, cx),
        )
        .keyboard_navigable(&self.focus_handle, &self.scroll_handle)
        .into_any_element()
    }
}

#[derive(Clone)]
struct PopoverKeyboardNavigation {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

pub(crate) struct Popover {
    id: ElementId,
    style: StyleRefinement,
    editor: Entity<InputState>,
    range: Range<usize>,
    width_limit: Range<Pixels>,
    content_id: ElementId,
    dismiss_behavior: PopoverDismissBehavior,
    keyboard_navigation: Option<PopoverKeyboardNavigation>,
    content_builder: Box<dyn Fn(&mut Window, &mut App) -> AnyElement>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PopoverDismissBehavior {
    #[default]
    Hover,
    SignatureHelp,
}

impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Popover {
    pub fn new<F, E>(
        id: impl Into<ElementId>,
        editor: Entity<InputState>,
        range: Range<usize>,
        f: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        Self {
            id: id.into(),
            editor,
            range,
            style: StyleRefinement::default(),
            width_limit: px(200.)..px(500.),
            content_id: "hover-popover-content".into(),
            dismiss_behavior: PopoverDismissBehavior::Hover,
            keyboard_navigation: None,
            content_builder: Box::new(move |window, cx| (f)(window, cx).into_any_element()),
        }
    }

    fn keyboard_navigable(
        mut self,
        focus_handle: &FocusHandle,
        scroll_handle: &ScrollHandle,
    ) -> Self {
        self.keyboard_navigation = Some(PopoverKeyboardNavigation {
            focus_handle: focus_handle.clone(),
            scroll_handle: scroll_handle.clone(),
        });
        self
    }

    pub(crate) fn dismiss_signature_help(mut self) -> Self {
        self.dismiss_behavior = PopoverDismissBehavior::SignatureHelp;
        self
    }

    pub(crate) fn content_id(mut self, id: impl Into<ElementId>) -> Self {
        self.content_id = id.into();
        self
    }

    /// Get the bounds of the range in the editor, if it is visible.
    fn trigger_bounds(&self, cx: &App) -> Option<Bounds<Pixels>> {
        let editor = self.editor.read(cx);
        let Some(last_layout) = editor.last_layout.as_ref() else {
            return None;
        };

        let Some(last_bounds) = editor.last_bounds else {
            return None;
        };

        let (_, _, start_pos) = editor.line_and_position_for_offset(self.range.start);
        let (_, _, end_pos) = editor.line_and_position_for_offset(self.range.end);

        let Some(start_pos) = start_pos else {
            return None;
        };
        let Some(end_pos) = end_pos else {
            return None;
        };

        Some(Bounds::from_corners(
            last_bounds.origin + start_pos,
            last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
        ))
    }
}

impl IntoElement for Popover {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct PopoverLayoutState {
    bounds: Bounds<Pixels>,
    element: Option<AnyElement>,
}

impl Element for Popover {
    type RequestLayoutState = PopoverLayoutState;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let trigger_bounds = match self.trigger_bounds(cx) {
            Some(bounds) => bounds,
            None => {
                return (
                    div().into_any_element().request_layout(window, cx),
                    PopoverLayoutState {
                        bounds: Bounds::default(),
                        element: None,
                    },
                );
            }
        };

        let max_width = self
            .width_limit
            .end
            .min(window.bounds().size.width - SNAP_TO_EDGE * 2)
            .max(px(200.));
        let max_height = (window.bounds().size.height - SNAP_TO_EDGE * 2).min(px(320.));

        let mut container = div()
            .id(self.content_id.clone())
            .flex_none()
            .occlude()
            .p_1()
            .text_xs()
            .popover_style(cx)
            .shadow_md()
            .max_w(max_width)
            .max_h(max_height)
            .overflow_y_scroll()
            .refine_style(&self.style);
        if let Some(navigation) = self.keyboard_navigation.as_ref() {
            let focus_handle = navigation.focus_handle.clone();
            let scroll_handle = navigation.scroll_handle.clone();
            let editor = self.editor.clone();
            container = container
                .track_focus(&focus_handle)
                .track_scroll(&scroll_handle)
                .on_key_down(move |event, window, cx| {
                    handle_hover_key_down(
                        event,
                        &focus_handle,
                        &scroll_handle,
                        &editor,
                        window,
                        cx,
                    );
                });
        }
        let mut popover =
            deferred(container.child((self.content_builder)(window, cx))).into_any_element();

        let popover_size = popover.layout_as_root(AvailableSpace::min_size(), window, cx);
        const SNAP_TO_EDGE: Pixels = px(8.);
        let top_space = trigger_bounds.top() - SNAP_TO_EDGE;
        let right_space = window.bounds().size.width - trigger_bounds.left() - SNAP_TO_EDGE;

        let mut pos = point(
            trigger_bounds.left(),
            trigger_bounds.top() - popover_size.height,
        );
        if popover_size.height > top_space {
            pos.y = trigger_bounds.bottom();
        }
        if popover_size.width > right_space {
            pos.x = trigger_bounds.right() - popover_size.width;
        }

        let mut empty = div().into_any_element();
        let layout_id = empty.request_layout(window, cx);
        (
            layout_id,
            PopoverLayoutState {
                bounds: Bounds {
                    origin: pos,
                    size: popover_size,
                },
                element: Some(popover),
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let bounds = request_layout.bounds;
        let Some(popover) = request_layout.element.as_mut() else {
            return;
        };

        window.with_absolute_element_offset(bounds.origin, |window| {
            popover.prepaint(window, cx);
        })
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let bounds = request_layout.bounds;
        let Some(popover) = request_layout.element.as_mut() else {
            return;
        };

        popover.paint(window, cx);

        let editor = self.editor.clone();
        let dismiss_behavior = self.dismiss_behavior;
        // Mouse down out to hide.
        window.on_mouse_event(move |event: &MouseDownEvent, _, _, cx| {
            if !bounds.contains(&event.position) {
                let _ = editor.update(cx, |editor, cx| match dismiss_behavior {
                    PopoverDismissBehavior::Hover => editor.clear_hover_state(cx),
                    PopoverDismissBehavior::SignatureHelp => {
                        editor.close_signature_help(cx);
                    }
                });
            }
        });

        // Mouse out of trigger + popover bounds
        if self.dismiss_behavior == PopoverDismissBehavior::Hover {
            let editor = self.editor.clone();
            let focus_handle = self
                .keyboard_navigation
                .as_ref()
                .map(|navigation| navigation.focus_handle.clone());
            let trigger_bounds = self.trigger_bounds(cx).unwrap_or(bounds);
            let keep_open_region = trigger_bounds.union(&bounds);
            window.on_mouse_event(move |event: &MouseMoveEvent, _, window, cx| {
                if focus_handle
                    .as_ref()
                    .is_some_and(|focus_handle| focus_handle.is_focused(window))
                {
                    return;
                }
                if !keep_open_region.contains(&event.position) {
                    let _ = editor.update(cx, |editor, cx| {
                        editor.clear_hover_state(cx);
                    });
                }
            })
        }
    }
}

fn scroll_hover_handle(scroll_handle: &ScrollHandle, command: HoverPopoverScroll) -> bool {
    let old_offset = scroll_handle.offset();
    let max_offset = scroll_handle.max_offset();
    let viewport_height = scroll_handle.bounds().size.height.max(px(18.));
    let delta = match command {
        HoverPopoverScroll::LineUp => px(-18.),
        HoverPopoverScroll::LineDown => px(18.),
        HoverPopoverScroll::PageUp => -viewport_height,
        HoverPopoverScroll::PageDown => viewport_height,
        HoverPopoverScroll::Top => {
            scroll_handle.set_offset(point(old_offset.x, px(0.)));
            return old_offset.y != px(0.);
        }
        HoverPopoverScroll::Bottom => {
            scroll_handle.set_offset(point(old_offset.x, -max_offset.y));
            return old_offset.y != -max_offset.y;
        }
    };
    let next_y = (old_offset.y - delta).clamp(-max_offset.y, px(0.));
    scroll_handle.set_offset(point(old_offset.x, next_y));
    next_y != old_offset.y
}

fn handle_hover_key_down(
    event: &KeyDownEvent,
    focus_handle: &FocusHandle,
    scroll_handle: &ScrollHandle,
    editor: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) {
    if !focus_handle.is_focused(window) {
        return;
    }

    let key = event.keystroke.key.as_str();
    let modifiers = &event.keystroke.modifiers;
    let command = match key {
        "up" if modifiers.secondary() => Some(HoverPopoverScroll::Top),
        "down" if modifiers.secondary() => Some(HoverPopoverScroll::Bottom),
        "up" if modifiers.alt => Some(HoverPopoverScroll::PageUp),
        "down" if modifiers.alt => Some(HoverPopoverScroll::PageDown),
        "up" => Some(HoverPopoverScroll::LineUp),
        "down" => Some(HoverPopoverScroll::LineDown),
        "pageup" => Some(HoverPopoverScroll::PageUp),
        "pagedown" => Some(HoverPopoverScroll::PageDown),
        "home" => Some(HoverPopoverScroll::Top),
        "end" => Some(HoverPopoverScroll::Bottom),
        "escape" => {
            let _ = editor.update(cx, |editor, cx| {
                editor.clear_hover_state(cx);
                editor.focus_handle.focus(window, cx);
            });
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        _ => None,
    };

    let Some(command) = command else {
        return;
    };
    scroll_hover_handle(scroll_handle, command);
    let _ = editor.update(cx, |_, cx| cx.notify());
    window.prevent_default();
    cx.stop_propagation();
}
