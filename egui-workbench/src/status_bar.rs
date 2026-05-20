//! Status bar — thin horizontal strip with appendable cells. See
//! `DESIGN.md`. Phase E will populate the cell rendering; Phase C
//! gives hosts a place to draw via `WorkbenchBehavior::status_bar_ui`.

pub struct StatusBar {
    pub visible: bool,
}

impl Default for StatusBar {
    fn default() -> Self {
        Self { visible: true }
    }
}
