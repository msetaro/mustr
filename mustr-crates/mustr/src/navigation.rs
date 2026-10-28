//! Moving between mustr's top-level destinations: the home view and open reviews. Used by the
//! title bar's tabs and the icon rail.

use crate::home::HomeView;
use gpui::{AppContext as _, Context, Window};
use workspace::Workspace;

/// Activates the home view, reopening it if it was closed.
pub fn show_home(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<HomeView>().next());
    if let Some(home) = existing {
        workspace.activate_item(&home, true, true, window, cx);
        return;
    }
    let home = cx.new(|cx| HomeView::new(workspace.weak_handle(), window, cx));
    // Home goes back in the first tab.
    workspace.active_pane().update(cx, |pane, cx| {
        pane.add_item(Box::new(home), true, true, Some(0), window, cx)
    });
}
