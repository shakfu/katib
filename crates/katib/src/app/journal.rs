//! Daily notes: `<journal folder>/<YYYY-MM-DD>.md`, opened from a calendar.

use gpui_kit::component::{
    IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    calendar::{Calendar, CalendarEvent, CalendarState},
    popover::Popover,
};
use gpui_kit::*;

use super::Katib;

impl Katib {
    pub(super) fn open_today(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let today = jiff::ZonedDateTime::now().date();
        self.open_day(&today.to_string(), window, cx);
    }

    /// Open the daily note for `day` (`2026-11-24`), creating it if needed.
    fn open_day(&mut self, day: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = format!("{}/{day}", self.config.journal);
        let exists = self.vault.entry(&id).is_ok_and(|e| e.path.is_file());
        if exists {
            self.open(&id, window, cx);
        } else {
            self.create(&id, false, window, cx);
        }
    }

    pub(super) fn calendar_event(
        &mut self,
        _: &Entity<CalendarState>,
        event: &CalendarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let CalendarEvent::Selected(date) = event;
        if let Some(day) = date.format("%Y-%m-%d") {
            self.calendar_open = false;
            self.open_day(&day, window, cx);
        }
    }

    /// The toolbar's calendar: pick a day to open its note.
    pub(super) fn calendar_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.calendar.clone();
        let this = cx.weak_entity();
        Popover::new("calendar")
            .open(self.calendar_open)
            .on_open_change(move |open, _, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.calendar_open = *open;
                    cx.notify();
                });
            })
            .trigger(
                Button::new("calendar-button")
                    .ghost()
                    .small()
                    .icon(IconName::Calendar)
                    .tooltip("Daily notes"),
            )
            .content(move |_, _, _| Calendar::new(&state).small())
    }
}
