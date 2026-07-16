use std::ops::Range;

use gpui::{
    Along, App, Axis, Bounds, Context, ElementId, EventEmitter, IsZero, Pixels, Window, px,
};

mod panel;
mod resize_handle;
pub use panel::*;
pub(crate) use resize_handle::*;

pub(crate) const PANEL_MIN_SIZE: Pixels = px(100.);

/// Create a [`ResizablePanelGroup`] with horizontal resizing
pub fn h_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup {
    ResizablePanelGroup::new(id).axis(Axis::Horizontal)
}

/// Create a [`ResizablePanelGroup`] with vertical resizing
pub fn v_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup {
    ResizablePanelGroup::new(id).axis(Axis::Vertical)
}

/// Create a [`ResizablePanel`].
pub fn resizable_panel() -> ResizablePanel {
    ResizablePanel::new()
}

/// State for a [`ResizablePanel`]
#[derive(Debug, Clone)]
pub struct ResizableState {
    /// The `axis` will sync to actual axis of the ResizablePanelGroup in use.
    axis: Axis,
    panels: Vec<ResizablePanelState>,
    sizes: Vec<Pixels>,
    pub(crate) resizing_panel_ix: Option<usize>,
    pending_resize: Option<(usize, Pixels)>,
    bounds: Bounds<Pixels>,
}

impl Default for ResizableState {
    fn default() -> Self {
        Self {
            axis: Axis::Horizontal,
            panels: vec![],
            sizes: vec![],
            resizing_panel_ix: None,
            pending_resize: None,
            bounds: Bounds::default(),
        }
    }
}

impl ResizableState {
    /// Get the size of the panels.
    pub fn sizes(&self) -> &Vec<Pixels> {
        &self.sizes
    }

    /// Get the last painted bounds for a panel.
    ///
    /// This lets automation and accessibility tooling locate a resize handle
    /// without duplicating the panel group's layout math. A handle at index
    /// `ix - 1` lies on the leading edge of panel `ix`.
    pub fn panel_bounds(&self, ix: usize) -> Option<Bounds<Pixels>> {
        self.panels.get(ix).map(|panel| panel.bounds)
    }

    /// Programmatically resize the panel at `ix` to `size`, redistributing
    /// space among siblings using the same logic as a drag.
    ///
    /// Sizes are clamped to the panel's `size_range` and to the container.
    /// Emits `ResizablePanelEvent::Resized` so subscribers (e.g. preference
    /// persistence) see the change just as if the user had dragged a handle.
    ///
    /// Out-of-range indices are a no-op. For the last panel, space is taken
    /// from the previous sibling (the last panel has no handle of its own).
    pub fn resize_panel(
        &mut self,
        ix: usize,
        size: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.sizes.len() {
            return;
        }
        if ix + 1 < self.sizes.len() {
            self.resize_panel_at_handle(ix, size, window, cx);
        } else if ix > 0 {
            // Last panel: drive its size by resizing the previous sibling so
            // the freed space lands here.
            let delta = self.sizes[ix] - size;
            let prev = self.sizes[ix - 1];
            self.resize_panel_at_handle(ix - 1, prev + delta, window, cx);
        }
        self.done_resizing(cx);
    }

    pub(crate) fn insert_panel(
        &mut self,
        size: Option<Pixels>,
        ix: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let panel_state = ResizablePanelState {
            size,
            ..Default::default()
        };

        let size = size.unwrap_or(PANEL_MIN_SIZE);

        // We make sure that the size always sums up to the container size
        // by reducing the size of all other panels first.
        let container_size = self.container_size().max(px(1.));
        let total_leftover_size = (container_size - size).max(px(1.));

        for (i, panel) in self.panels.iter_mut().enumerate() {
            let ratio = self.sizes[i] / container_size;
            self.sizes[i] = total_leftover_size * ratio;
            panel.size = Some(self.sizes[i]);
        }

        if let Some(ix) = ix {
            self.panels.insert(ix, panel_state);
            self.sizes.insert(ix, size);
        } else {
            self.panels.push(panel_state);
            self.sizes.push(size);
        };

        cx.notify();
    }

    pub(crate) fn sync_panels_count(
        &mut self,
        axis: Axis,
        panels_count: usize,
        cx: &mut Context<Self>,
    ) {
        let mut changed = self.axis != axis;
        self.axis = axis;

        if panels_count > self.panels.len() {
            let diff = panels_count - self.panels.len();
            self.panels
                .extend(vec![ResizablePanelState::default(); diff]);
            self.sizes.extend(vec![PANEL_MIN_SIZE; diff]);
            changed = true;
        }

        if panels_count < self.panels.len() {
            self.panels.truncate(panels_count);
            self.sizes.truncate(panels_count);
            changed = true;
        }

        if changed {
            // We need to make sure the total size is in line with the container size.
            self.adjust_to_container_size(cx);
        }
    }

    pub(crate) fn update_panel_size(
        &mut self,
        panel_ix: usize,
        bounds: Bounds<Pixels>,
        size_range: Range<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.sync_panel_geometry(panel_ix, bounds, size_range) {
            return;
        }
        cx.notify();
    }

    fn sync_panel_geometry(
        &mut self,
        panel_ix: usize,
        bounds: Bounds<Pixels>,
        size_range: Range<Pixels>,
    ) -> bool {
        let size = bounds.size.along(self.axis);
        // This check is only necessary to stop the very first panel from resizing on its own
        // it needs to be passed when the panel is freshly created so we get the initial size,
        // but its also fine when it sometimes passes later.
        let seeds_initial_size = self.sizes[panel_ix].as_f32() == PANEL_MIN_SIZE.as_f32()
            && (self.sizes[panel_ix] != size || self.panels[panel_ix].size != Some(size));
        let geometry_changed = seeds_initial_size
            || self.panels[panel_ix].bounds != bounds
            || self.panels[panel_ix].size_range != size_range;
        if !geometry_changed {
            return false;
        }

        if seeds_initial_size {
            self.sizes[panel_ix] = size;
            self.panels[panel_ix].size = Some(size);
        }
        self.panels[panel_ix].bounds = bounds;
        self.panels[panel_ix].size_range = size_range;
        true
    }

    pub(crate) fn remove_panel(&mut self, panel_ix: usize, cx: &mut Context<Self>) {
        self.panels.remove(panel_ix);
        self.sizes.remove(panel_ix);
        if let Some(resizing_panel_ix) = self.resizing_panel_ix {
            if resizing_panel_ix > panel_ix {
                self.resizing_panel_ix = Some(resizing_panel_ix - 1);
            }
        }
        self.adjust_to_container_size(cx);
    }

    pub(crate) fn replace_panel(
        &mut self,
        panel_ix: usize,
        panel: ResizablePanelState,
        cx: &mut Context<Self>,
    ) {
        let old_size = self.sizes[panel_ix];

        self.panels[panel_ix] = panel;
        self.sizes[panel_ix] = old_size;
        self.adjust_to_container_size(cx);
    }

    pub(crate) fn clear(&mut self) {
        self.panels.clear();
        self.sizes.clear();
    }

    #[inline]
    pub(crate) fn container_size(&self) -> Pixels {
        self.bounds.size.along(self.axis)
    }

    pub(crate) fn done_resizing(&mut self, cx: &mut Context<Self>) {
        self.resizing_panel_ix = None;
        cx.emit(ResizablePanelEvent::Resized);
    }

    /// Keep only the newest pointer position for a deferred resize.
    ///
    /// The final position is applied on mouse-up so expensive descendants are
    /// laid out once rather than once for every pointer event.
    pub(crate) fn queue_resize_panel_at_handle(&mut self, ix: usize, size: Pixels) {
        self.pending_resize = Some((ix, size));
    }

    fn take_queued_resize(&mut self) -> Option<(usize, Pixels)> {
        self.pending_resize.take()
    }

    pub(crate) fn flush_queued_resize(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((ix, size)) = self.take_queued_resize() else {
            return false;
        };
        self.resize_panel_at_handle(ix, size, window, cx)
    }

    fn panel_size_range(&self, ix: usize) -> Range<Pixels> {
        let Some(panel) = self.panels.get(ix) else {
            return PANEL_MIN_SIZE..Pixels::MAX;
        };

        panel.size_range.clone()
    }

    fn sync_real_panel_sizes(&mut self, _: &App) {
        for (i, panel) in self.panels.iter().enumerate() {
            self.sizes[i] = panel.bounds.size.along(self.axis);
        }
    }

    /// Resize the panel at `ix` by treating `ix` as the drag-handle position
    /// (the handle that sits between panel `ix` and panel `ix + 1`). Returns
    /// early on the last panel since there is no handle below it.
    ///
    /// This is the worker behind drag interactions and the public
    /// [`Self::resize_panel`] API.
    fn resize_panel_at_handle(
        &mut self,
        ix: usize,
        size: Pixels,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Only resize the left panels.
        if ix >= self.sizes.len().saturating_sub(1) {
            return false;
        }
        self.sync_real_panel_sizes(cx);
        let old_sizes = self.sizes.clone();

        let Some(new_sizes) = self.resized_panel_sizes(ix, size, &old_sizes) else {
            return false;
        };

        for (panel, size) in self.panels.iter_mut().zip(new_sizes.iter().copied()) {
            panel.size = Some(size);
        }
        self.sizes = new_sizes;
        cx.notify();
        true
    }

    /// Calculate a drag result while honoring both sides of the handle.
    ///
    /// The panel under the pointer can consume space only while the panels on
    /// the other side can release it, and vice versa. In particular, an
    /// expanding sibling's maximum is a hard stop. Returning `None` when the
    /// drag is already pinned at a constraint prevents redundant repaint work
    /// for every pointer event beyond that boundary.
    fn resized_panel_sizes(
        &self,
        ix: usize,
        requested_size: Pixels,
        old_sizes: &[Pixels],
    ) -> Option<Vec<Pixels>> {
        if ix >= old_sizes.len().saturating_sub(1) {
            return None;
        }

        let current_size = old_sizes[ix];
        if requested_size == current_size {
            return None;
        }

        let mut new_sizes = old_sizes.to_vec();
        if requested_size > current_size {
            let requested_growth = requested_size - current_size;
            let main_capacity = (self.panel_size_range(ix).end - current_size).max(px(0.));
            let release_capacity = ((ix + 1)..old_sizes.len()).fold(px(0.), |total, panel_ix| {
                total + (old_sizes[panel_ix] - self.panel_size_range(panel_ix).start).max(px(0.))
            });
            let mut remaining = requested_growth.min(main_capacity).min(release_capacity);
            if remaining == px(0.) {
                return None;
            }

            let growth = remaining;
            new_sizes[ix] += growth;
            for panel_ix in (ix + 1)..old_sizes.len() {
                let available =
                    (old_sizes[panel_ix] - self.panel_size_range(panel_ix).start).max(px(0.));
                let released = remaining.min(available);
                new_sizes[panel_ix] -= released;
                remaining -= released;
                if remaining == px(0.) {
                    break;
                }
            }
        } else {
            let requested_shrink = current_size - requested_size;
            let release_capacity = (0..=ix).rev().fold(px(0.), |total, panel_ix| {
                total + (old_sizes[panel_ix] - self.panel_size_range(panel_ix).start).max(px(0.))
            });
            let growth_capacity = ((ix + 1)..old_sizes.len()).fold(px(0.), |total, panel_ix| {
                total + (self.panel_size_range(panel_ix).end - old_sizes[panel_ix]).max(px(0.))
            });
            let mut remaining = requested_shrink.min(release_capacity).min(growth_capacity);
            if remaining == px(0.) {
                return None;
            }

            let shrink = remaining;
            for panel_ix in (0..=ix).rev() {
                let available =
                    (old_sizes[panel_ix] - self.panel_size_range(panel_ix).start).max(px(0.));
                let released = remaining.min(available);
                new_sizes[panel_ix] -= released;
                remaining -= released;
                if remaining == px(0.) {
                    break;
                }
            }

            let mut remaining = shrink;
            for panel_ix in (ix + 1)..old_sizes.len() {
                let available =
                    (self.panel_size_range(panel_ix).end - old_sizes[panel_ix]).max(px(0.));
                let accepted = remaining.min(available);
                new_sizes[panel_ix] += accepted;
                remaining -= accepted;
                if remaining == px(0.) {
                    break;
                }
            }
        }

        (new_sizes != old_sizes).then_some(new_sizes)
    }

    /// Adjust panel sizes according to the container size.
    ///
    /// When the container size changes, the panels should take up the same percentage as they did before.
    fn adjust_to_container_size(&mut self, cx: &mut Context<Self>) {
        if self.container_size().is_zero() {
            return;
        }

        let container_size = self.container_size();
        let total = self.sizes.iter().map(|s| s.as_f32()).sum::<f32>();
        if !total.is_finite() || total <= 0. {
            return;
        }
        let total_size = px(total);

        for i in 0..self.panels.len() {
            let size = self.sizes[i];
            let ratio = size / total_size;
            let new_size = container_size * ratio;

            self.sizes[i] = new_size;
            self.panels[i].size = Some(new_size);
        }
        cx.notify();
    }
}

impl EventEmitter<ResizablePanelEvent> for ResizableState {}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResizablePanelState {
    pub size: Option<Pixels>,
    pub size_range: Range<Pixels>,
    bounds: Bounds<Pixels>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn one_panel_state() -> ResizableState {
        ResizableState {
            axis: Axis::Vertical,
            panels: vec![ResizablePanelState::default()],
            sizes: vec![PANEL_MIN_SIZE],
            ..ResizableState::default()
        }
    }

    #[test]
    fn stable_panel_geometry_is_a_noop_after_initial_measurement() {
        let mut state = one_panel_state();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(640.), px(240.)));
        let range = px(120.)..px(480.);

        assert!(state.sync_panel_geometry(0, bounds, range.clone()));
        assert_eq!(state.sizes, vec![px(240.)]);
        assert!(!state.sync_panel_geometry(0, bounds, range));
    }

    #[test]
    fn panel_geometry_changes_only_when_bounds_or_constraints_change() {
        let mut state = one_panel_state();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(640.), px(240.)));
        let range = px(120.)..px(480.);
        assert!(state.sync_panel_geometry(0, bounds, range.clone()));

        let moved = Bounds::new(point(px(0.), px(12.)), size(px(640.), px(240.)));
        assert!(state.sync_panel_geometry(0, moved, range.clone()));
        assert!(!state.sync_panel_geometry(0, moved, range.clone()));

        assert!(state.sync_panel_geometry(0, moved, px(100.)..px(520.)));
        assert!(!state.sync_panel_geometry(0, moved, px(100.)..px(520.)));
    }

    #[test]
    fn panel_bounds_exposes_the_last_painted_geometry() {
        let mut state = one_panel_state();
        let bounds = Bounds::new(point(px(8.), px(12.)), size(px(640.), px(240.)));

        assert!(state.panel_bounds(0).is_some());
        assert_ne!(state.panel_bounds(0), Some(bounds));
        state.sync_panel_geometry(0, bounds, px(120.)..px(480.));

        assert_eq!(state.panel_bounds(0), Some(bounds));
        assert_eq!(state.panel_bounds(1), None);
    }

    #[test]
    fn resize_stops_when_expanding_sibling_reaches_its_maximum() {
        let state = ResizableState {
            axis: Axis::Vertical,
            panels: vec![
                ResizablePanelState {
                    size_range: px(120.)..Pixels::MAX,
                    ..Default::default()
                },
                ResizablePanelState {
                    size_range: px(104.)..px(420.),
                    ..Default::default()
                },
            ],
            sizes: vec![px(615.), px(280.)],
            ..ResizableState::default()
        };

        let resized = state
            .resized_panel_sizes(0, px(330.), &state.sizes)
            .expect("the dock can still grow to its maximum");
        assert_eq!(resized, vec![px(475.), px(420.)]);
        assert!(state.resized_panel_sizes(0, px(300.), &resized).is_none());
    }

    #[test]
    fn resize_distributes_growth_without_exceeding_later_panel_maximums() {
        let state = ResizableState {
            axis: Axis::Vertical,
            panels: vec![
                ResizablePanelState {
                    size_range: px(100.)..Pixels::MAX,
                    ..Default::default()
                },
                ResizablePanelState {
                    size_range: px(100.)..px(250.),
                    ..Default::default()
                },
                ResizablePanelState {
                    size_range: px(100.)..px(200.),
                    ..Default::default()
                },
            ],
            sizes: vec![px(300.), px(200.), px(100.)],
            ..ResizableState::default()
        };

        let resized = state
            .resized_panel_sizes(0, px(100.), &state.sizes)
            .expect("later panels can accept 150 pixels");
        assert_eq!(resized, vec![px(150.), px(250.), px(200.)]);
    }

    #[test]
    fn deferred_pointer_resize_keeps_only_the_latest_position_until_release() {
        let mut state = ResizableState::default();

        state.queue_resize_panel_at_handle(0, px(180.));
        state.queue_resize_panel_at_handle(0, px(220.));
        state.queue_resize_panel_at_handle(0, px(260.));
        assert_eq!(state.take_queued_resize(), Some((0, px(260.))));
        assert!(state.take_queued_resize().is_none());
        state.queue_resize_panel_at_handle(0, px(300.));
        assert_eq!(state.take_queued_resize(), Some((0, px(300.))));
    }
}
