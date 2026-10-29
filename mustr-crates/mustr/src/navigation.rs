//! Showing the Inbox's own page (`home.rs`), mustr's home. The rail's areas and the history are
//! in `areas.rs`.

use crate::home::HomeView;
use gpui::{AppContext as _, Context, Window};
use workspace::Workspace;

/// Shows the Inbox's page, reopening it if it was closed.
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
