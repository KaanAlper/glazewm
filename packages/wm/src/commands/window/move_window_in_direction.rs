use anyhow::Context;
use wm_common::{TilingDirection, WindowState, WmEvent};
use wm_platform::{Direction, Point, Rect};

use crate::{
  commands::container::{
    attach_container, detach_container, flatten_child_split_containers,
    flatten_split_container, move_container_within_tree,
    set_focused_descendant, wrap_in_split_container,
  },
  models::{
    Monitor, NonTilingWindow, SplitContainer,
    TilingContainer, TilingWindow, WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// The distance in pixels to snap the window to the monitor's edge.
const SNAP_DISTANCE: i32 = 15;

pub fn move_window_in_direction(
  window: WindowContainer,
  direction: &Direction,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  match window {
    WindowContainer::TilingWindow(window) => {
      move_tiling_window(window, direction, state, config)
    }
    WindowContainer::NonTilingWindow(non_tiling_window) => {
      match non_tiling_window.state() {
        WindowState::Floating(_) => {
          move_floating_window(non_tiling_window, direction, state)
        }
        WindowState::Fullscreen(_) => move_to_workspace_in_direction(
          &non_tiling_window.into(),
          direction,
          state,
        ),
        _ => Ok(()),
      }
    }
  }
}

/// Moves a tiling window like Hyprland's dwindle layout (`movewindow`).
///
/// A focal point is taken 1px outside the window's edge in the given
/// direction. The window is removed from the tree, and the window at the
/// focal point is split along its longer side; the moved window takes the
/// half that the focal point falls into. For example, in the layout
/// H[1 V[2 3]] where container 2 is moved left, this results in
/// H[V[2 1] 3].
///
/// Without a window in the given direction, the window is moved to the
/// monitor in that direction (if any).
fn move_tiling_window(
  window_to_move: TilingWindow,
  direction: &Direction,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let rect = window_to_move.to_rect()?;
  let focal_point = match direction {
    Direction::Up => Point {
      x: rect.left + rect.width() / 2,
      y: rect.top - 1,
    },
    Direction::Down => Point {
      x: rect.left + rect.width() / 2,
      y: rect.bottom + 1,
    },
    Direction::Left => Point {
      x: rect.left - 1,
      y: rect.top + rect.height() / 2,
    },
    Direction::Right => Point {
      x: rect.right + 1,
      y: rect.top + rect.height() / 2,
    },
  };

  let Some(target) =
    window_in_direction(&window_to_move, &focal_point, direction)?
  else {
    return move_to_workspace_in_direction(
      &window_to_move.into(),
      direction,
      state,
    );
  };

  let workspace = window_to_move.workspace().context("No workspace.")?;
  let had_focus = window_to_move.has_focus(None);

  // The focal point lies in a gap or at the target's edge, so it's
  // clamped into the target.
  dwindle_split(&window_to_move, &target, &focal_point, true, config)?;

  if had_focus {
    set_focused_descendant(&window_to_move.clone().into(), None);
    state.emit_event(WmEvent::FocusedContainerMoved {
      focused_container: window_to_move.to_dto()?,
    });
  }

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children())
    .queue_cursor_jump();

  Ok(())
}

/// Re-inserts `window` by splitting `target` (Hyprland's dwindle
/// `onWindowRemovedTiling` + `onWindowCreatedTiling`).
///
/// The window is removed first, so that the target is measured the way
/// it'll be split. The target is split along its longer side, and the
/// window takes the half that `point` falls into. With `clamp_point`, a
/// point outside the target (e.g. in a gap) is clamped into it; otherwise
/// such a point selects the second half, as in Hyprland.
pub fn dwindle_split(
  window: &TilingWindow,
  target: &TilingWindow,
  point: &Point,
  clamp_point: bool,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace = target.workspace().context("No workspace.")?;
  let window_to_move = window.clone();

  // The window may already be detached (e.g. a new window).
  if let Some(old_parent) = window.parent() {
    detach_container(window_to_move.clone().into())?;

    if let Some(old_parent) = old_parent.as_split().cloned() {
      if old_parent.child_count() == 1 && old_parent.parent().is_some() {
        flatten_split_container(old_parent)?;
      }
    }
  }

  let target_rect = target.to_rect()?;
  let side_by_side = target_rect.width() > target_rect.height();

  let is_first = if clamp_point {
    if side_by_side {
      point.x.clamp(target_rect.left, target_rect.right)
        < target_rect.left + target_rect.width() / 2
    } else {
      point.y.clamp(target_rect.top, target_rect.bottom)
        < target_rect.top + target_rect.height() / 2
    }
  } else {
    target_rect.contains_point(point)
      && if side_by_side {
        point.x < target_rect.left + target_rect.width() / 2
      } else {
        point.y < target_rect.top + target_rect.height() / 2
      }
  };

  let split_direction = if side_by_side {
    TilingDirection::Horizontal
  } else {
    TilingDirection::Vertical
  };

  let target_parent = target
    .direction_container()
    .context("No direction container.")?;

  if target.tiling_siblings().count() == 0 {
    // Target fills its parent (e.g. alone on the workspace), so split the
    // parent itself.
    target_parent.set_tiling_direction(split_direction);

    attach_container(
      &window_to_move.clone().into(),
      &target_parent.clone().into(),
      Some(target.index() + usize::from(!is_first)),
    )?;
  } else {
    let split_container =
      SplitContainer::new(split_direction, config.value.gaps.clone());

    wrap_in_split_container(
      &split_container,
      &target_parent.clone().into(),
      &[target.clone().into()],
    )?;

    attach_container(
      &window_to_move.clone().into(),
      &split_container.into(),
      Some(usize::from(!is_first)),
    )?;
  }

  flatten_child_split_containers(&target_parent.into())?;
  flatten_child_split_containers(&workspace.into())?;

  Ok(())
}

/// Gets the tiling window on the same workspace at the focal point.
///
/// Since the focal point may land in the gap between windows, the nearest
/// window in the given direction that spans the focal point is used
/// instead when none contains it.
fn window_in_direction(
  window: &TilingWindow,
  focal_point: &Point,
  direction: &Direction,
) -> anyhow::Result<Option<TilingWindow>> {
  let workspace = window.workspace().context("No workspace.")?;
  let mut nearest: Option<(i32, TilingWindow)> = None;

  for other in workspace.descendants() {
    let Ok(TilingContainer::TilingWindow(other)) =
      other.as_tiling_container()
    else {
      continue;
    };

    if other.id() == window.id() {
      continue;
    }

    let r = other.to_rect()?;

    if r.contains_point(focal_point) {
      return Ok(Some(other));
    }

    let distance = match direction {
      Direction::Left if (r.top..=r.bottom).contains(&focal_point.y) => {
        focal_point.x - r.right
      }
      Direction::Right if (r.top..=r.bottom).contains(&focal_point.y) => {
        r.left - focal_point.x
      }
      Direction::Up if (r.left..=r.right).contains(&focal_point.x) => {
        focal_point.y - r.bottom
      }
      Direction::Down if (r.left..=r.right).contains(&focal_point.x) => {
        r.top - focal_point.y
      }
      _ => continue,
    };

    if distance >= 0 && nearest.as_ref().is_none_or(|(d, _)| distance < *d)
    {
      nearest = Some((distance, other));
    }
  }

  Ok(nearest.map(|(_, window)| window))
}

fn move_to_workspace_in_direction(
  window_to_move: &WindowContainer,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let parent = window_to_move.parent().context("No parent.")?;
  let workspace = window_to_move.workspace().context("No workspace.")?;
  let monitor = parent.monitor().context("No monitor.")?;

  let target_workspace = state
    .monitor_in_direction(&monitor, direction)?
    .and_then(|monitor| monitor.displayed_workspace());

  if let Some(target_workspace) = target_workspace {
    // Since the window is crossing monitors, adjustments might need to be
    // made because of DPI.
    if monitor.has_dpi_difference(&target_workspace.clone().into())? {
      window_to_move.set_has_pending_dpi_adjustment(true);
    }

    // Update floating placement since the window has to cross monitors.
    window_to_move.set_floating_placement(
      window_to_move
        .floating_placement()
        .translate_to_center(&target_workspace.to_rect()?),
    );

    if let WindowContainer::NonTilingWindow(window_to_move) =
      &window_to_move
    {
      window_to_move.set_insertion_target(None);
    }

    let target_index = match direction {
      Direction::Down | Direction::Right => 0,
      _ => target_workspace.child_count(),
    };

    // Focus should be reassigned within the original workspace after the
    // window is moved out. For example, if the focus order is 1. tiling
    // window and 2. fullscreen window, then we'd want to retain focus on a
    // tiling window on move.
    let focus_target = state.focus_target_after_removal(window_to_move);

    move_container_within_tree(
      &window_to_move.clone().into(),
      &target_workspace.clone().into(),
      target_index,
      state,
    )?;

    if let Some(focus_target) = focus_target {
      set_focused_descendant(
        &focus_target,
        Some(&workspace.clone().into()),
      );
    }

    state
      .pending_sync
      .queue_container_to_redraw(window_to_move.clone())
      .queue_containers_to_redraw(target_workspace.tiling_children())
      .queue_containers_to_redraw(parent.tiling_children())
      .queue_cursor_jump()
      .queue_workspace_to_reorder(target_workspace);
  }

  Ok(())
}

fn move_floating_window(
  window_to_move: NonTilingWindow,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let new_position =
    new_floating_position(&window_to_move, direction, state)?;

  if let Some((position_rect, target_monitor)) = new_position {
    let monitor = window_to_move.monitor().context("No monitor.")?;

    // Mark window as needing DPI adjustment if it crosses monitors. The
    // handler for `PlatformEvent::LocationChanged` will update the
    // window's workspace if it goes out of bounds of its current
    // workspace.
    if monitor.id() != target_monitor.id()
      && monitor.has_dpi_difference(&target_monitor.into())?
    {
      window_to_move.set_has_pending_dpi_adjustment(true);
    }

    window_to_move.set_floating_placement(position_rect);
    state.pending_sync.queue_container_to_redraw(window_to_move);
  }

  Ok(())
}

/// Returns a tuple of the new floating position and the target monitor.
fn new_floating_position(
  window_to_move: &NonTilingWindow,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<Option<(Rect, Monitor)>> {
  let monitor = window_to_move.monitor().context("No monitor.")?;
  let monitor_rect = monitor.native_properties().working_area;
  let window_pos = window_to_move.native_properties().frame;

  let is_on_monitor_edge = match direction {
    Direction::Up => window_pos.top == monitor_rect.top,
    Direction::Down => window_pos.bottom == monitor_rect.bottom,
    Direction::Left => window_pos.left == monitor_rect.left,
    Direction::Right => window_pos.right == monitor_rect.right,
  };

  // Window is on the edge of the monitor and should be moved to a
  // different monitor in the given direction.
  if is_on_monitor_edge {
    let next_monitor = state.monitor_in_direction(&monitor, direction)?;

    if let Some(next_monitor) = next_monitor {
      let monitor_rect = next_monitor.native().working_area()?.clone();

      let position = snap_to_monitor_edge(
        &window_pos,
        &monitor_rect,
        &direction.inverse(),
      )
      .clamp(&monitor_rect);

      return Ok(Some((position, next_monitor)));
    }

    return Ok(None);
  }

  let (monitor_length, window_length) = match direction {
    Direction::Up | Direction::Down => {
      (monitor_rect.height(), window_pos.height())
    }
    _ => (monitor_rect.width(), window_pos.width()),
  };

  let length_delta = monitor_length - window_length;

  // Calculate the distance the window should move based on the ratio of
  // the window's length to the monitor's length.
  #[allow(clippy::cast_precision_loss)]
  let move_distance = match window_length as f32 / monitor_length as f32 {
    x if (0.0..0.2).contains(&x) => length_delta / 5,
    x if (0.2..0.4).contains(&x) => length_delta / 4,
    x if (0.4..0.6).contains(&x) => length_delta / 3,
    _ => length_delta / 2,
  };

  // Snap the window to the current monitor's edge if it's within 15px of
  // it after the move.
  let should_snap_to_edge = match direction {
    Direction::Up => {
      window_pos.top - move_distance - SNAP_DISTANCE < monitor_rect.top
    }
    Direction::Down => {
      window_pos.bottom + move_distance + SNAP_DISTANCE
        > monitor_rect.bottom
    }
    Direction::Left => {
      window_pos.left - move_distance - SNAP_DISTANCE < monitor_rect.left
    }
    Direction::Right => {
      window_pos.right + move_distance + SNAP_DISTANCE > monitor_rect.right
    }
  };

  if should_snap_to_edge {
    let position =
      snap_to_monitor_edge(&window_pos, &monitor_rect, direction);

    return Ok(Some((position, monitor)));
  }

  // Snap the window to the current monitor's inverse edge if it's in
  // between two monitors or outside the bounds of the current monitor.
  let should_snap_to_inverse_edge = match direction {
    Direction::Up => window_pos.bottom > monitor_rect.bottom,
    Direction::Down => window_pos.top < monitor_rect.top,
    Direction::Left => window_pos.right > monitor_rect.right,
    Direction::Right => window_pos.left < monitor_rect.left,
  };

  let position = if should_snap_to_inverse_edge {
    snap_to_monitor_edge(&window_pos, &monitor_rect, &direction.inverse())
  } else {
    window_pos.translate_in_direction(direction, move_distance)
  };

  Ok(Some((position, monitor)))
}

fn snap_to_monitor_edge(
  window_pos: &Rect,
  monitor_rect: &Rect,
  edge: &Direction,
) -> Rect {
  let (x, y) = match edge {
    Direction::Up => (window_pos.x(), monitor_rect.top),
    Direction::Down => {
      (window_pos.x(), monitor_rect.bottom - window_pos.height())
    }
    Direction::Left => (monitor_rect.left, window_pos.y()),
    Direction::Right => {
      (monitor_rect.right - window_pos.width(), window_pos.y())
    }
  };

  window_pos.translate_to_coordinates(x, y)
}
