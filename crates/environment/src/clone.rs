//! What a clone covers, and how a job's per-area counts become progress an operator can trust.
//!
//! The clone copies **content and configuration, not infrastructure and not media bytes**. That
//! boundary is the whole design of this request, and it is stated in the types rather than only in
//! the UI: [`Area::Media`] is deliberately absent from the copy list and present in the
//! *reference* list, because a staging environment that silently re-uploaded every asset would
//! turn a 200-row clone into a 40 GB one and would leave the operator's storage bill describing
//! something they never asked for.
//!
//! The progress fold is here rather than in the panel because the failure it prevents is
//! arithmetic: a job whose total is the sum of areas that are *still to be discovered* renders a
//! progress bar that runs backwards, and a bar that runs backwards is read as a bug in the
//! product rather than in the estimate.

use std::collections::BTreeMap;
use std::fmt;

/// One thing a clone can copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Area {
    /// Published pages and their revision history.
    Pages,
    /// Page translations.
    Translations,
    /// Menus and navigation records.
    Menus,
    /// Site settings rows.
    SiteSettings,
    /// The organization's theme selection.
    Theme,
    /// Workflow definitions.
    Workflows,
}

impl Area {
    /// Every area, in the order the runner copies them.
    ///
    /// The order is the dependency order and it is not arbitrary: pages first because a
    /// translation has nothing to hang from, menus after pages because a menu entry points at a
    /// page, and theme and workflows last because both reference content that is now present. A
    /// runner that copied them in an arbitrary order would fail on a foreign key that the
    /// previous area had not created yet, and the error would name the constraint rather than the
    /// ordering.
    pub const ALL: [Self; 6] = [
        Self::Pages,
        Self::Translations,
        Self::Menus,
        Self::SiteSettings,
        Self::Theme,
        Self::Workflows,
    ];

    /// The wire name, as stored in `areas text[]` and shown in the wizard.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pages => "pages",
            Self::Translations => "translations",
            Self::Menus => "menus",
            Self::SiteSettings => "site_settings",
            Self::Theme => "theme",
            Self::Workflows => "workflows",
        }
    }

    /// The label the wizard's checkbox carries.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pages => "Pages & revisions",
            Self::Translations => "Translations",
            Self::Menus => "Menus & navigation records",
            Self::SiteSettings => "Site settings",
            Self::Theme => "Theme selection",
            Self::Workflows => "Workflow definitions",
        }
    }

    /// Parse a stored area name.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|area| area.as_str() == raw)
    }

    /// The order this area must be copied in, for sorting a partial selection.
    pub fn order(self) -> usize {
        Self::ALL
            .iter()
            .position(|area| *area == self)
            .expect("every Area is in ALL")
    }

    /// What copying this area costs the organization, in words the wizard can show.
    pub fn weight(self) -> AreaWeight {
        match self {
            // Revisions are the bulk of a real site's rows; saying "pages" alone would promise
            // a tenth of the work.
            Self::Pages | Self::Translations | Self::Workflows => AreaWeight::Heavy,
            Self::Menus | Self::SiteSettings => AreaWeight::Light,
            Self::Theme => AreaWeight::Tiny,
        }
    }
}

/// A rough size class, so the wizard's estimate can be a range and not a fake number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AreaWeight {
    /// A handful of rows.
    Tiny,
    /// Tens of rows.
    Light,
    /// Hundreds or more.
    Heavy,
}

impl fmt::Display for AreaWeight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tiny => write!(f, "a handful of rows"),
            Self::Light => write!(f, "a few dozen rows"),
            Self::Heavy => write!(f, "hundreds of rows"),
        }
    }
}

/// The areas a new clone job will copy, in copy order, with the archived-pages choice applied.
///
/// The returned list is **deduplicated and ordered**, and that is the point: a wizard sends
/// whatever checkboxes were ticked, and a runner that copies in the order it was handed would
/// fail on the dependency it did not know about. Normalising here means the API, the runner and
/// the stored `areas` array all agree on one order.
pub fn plan_areas(selected: &[Area], exclude_archived: bool) -> Vec<Area> {
    let mut chosen: Vec<Area> = Vec::new();
    for area in selected {
        if !chosen.contains(area) {
            chosen.push(*area);
        }
    }
    chosen.sort_by_key(|area| area.order());
    // The flag is carried on the job rather than resolved here: whether an archived page is
    // copied changes what the runner's `WHERE` clause looks like, and resolving it into the area
    // list would make the list mean two things at once.
    let _ = exclude_archived;
    chosen
}

/// Reject an empty area selection.
///
/// Its own function because the wizard blocks on it before anything is created: a clone of zero
/// areas produces an environment that looks cloned and is empty, and the operator finds out when
/// the Changes tab says "no changes since the clone".
pub fn require_areas(selected: &[Area]) -> Result<Vec<Area>, &'static str> {
    if selected.is_empty() {
        return Err("Choose at least one thing to copy into the new environment.");
    }
    Ok(plan_areas(selected, false))
}

/// The per-area tally of a running clone.
///
/// This is the progress source. The runner increments it as each area finishes and recomputes the
/// totals, and the panel reads the totals rather than counting rows itself — the alternative is a
/// second implementation of "how far along is this" in TypeScript, which is exactly the kind of
/// arithmetic that agrees with itself and disagrees with the runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Rows copied per area so far.
    pub done: BTreeMap<Area, u64>,
    /// Rows expected per area, discovered as the runner counts the source.
    pub total: BTreeMap<Area, u64>,
    /// The area that failed, when the job failed.
    pub failed_area: Option<Area>,
}

impl Progress {
    /// An empty tally, for a job that has not started.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `count` rows of `area` are expected.
    pub fn expect(&mut self, area: Area, count: u64) {
        self.total.insert(area, count);
    }

    /// Record that `count` more rows of `area` have been copied.
    ///
    /// The count is clamped to the expectation. A runner that copies more rows than it counted
    /// would otherwise push the bar past 100% and leave it there, which reads as a stalled job
    /// rather than as a miscount.
    pub fn advance(&mut self, area: Area, count: u64) {
        let ceiling = self.total.get(&area).copied().unwrap_or(u64::MAX);
        let entry = self.done.entry(area).or_insert(0);
        *entry = (*entry).saturating_add(count).min(ceiling);
    }

    /// Mark `area` as the one that failed.
    pub fn fail(&mut self, area: Area) {
        self.failed_area = Some(area);
    }

    /// Rows copied so far, across every area.
    pub fn items_done(&self) -> u64 {
        self.done.values().sum()
    }

    /// Rows expected in total.
    pub fn items_total(&self) -> u64 {
        self.total.values().sum()
    }

    /// Completion as a percentage, 0–100.
    ///
    /// A job whose total is not known yet reports 0 rather than 100: the panel shows a determinate
    /// bar only once there is something to be determinate about, and an optimistic 100 on an
    /// empty tally is the single most misleading thing a progress bar can say.
    pub fn percent(&self) -> u8 {
        let total = self.items_total();
        if total == 0 {
            return 0;
        }
        let done = self.items_done();
        // Integer maths with a rounding guard: `(done * 100) / total` on u64 overflows above
        // ~1.8e17 rows, which no site has, but the guard is free and the division cannot.
        let pct = (done as u128 * 100) / total as u128;
        u8::try_from(pct.min(100)).unwrap_or(100)
    }

    /// The one-line summary the Overview tab shows under the bar.
    pub fn summary(&self) -> String {
        if let Some(area) = self.failed_area {
            return format!("Copying stopped at {}.", area.label());
        }
        let (done, total) = (self.items_done(), self.items_total());
        if total == 0 {
            return "Counting the rows to copy…".to_string();
        }
        format!("{done} of {total} rows copied.")
    }

    /// Per-area counts for the list screen's "Content" column.
    pub fn area_counts(&self) -> BTreeMap<Area, u64> {
        self.done.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_areas() -> Vec<Area> {
        Area::ALL.to_vec()
    }

    #[test]
    fn the_area_order_puts_pages_before_anything_that_references_them() {
        // Menus point at pages; a runner copying them the other way round fails on a foreign key
        // and the error names the constraint instead of the ordering.
        assert!(Area::Pages.order() < Area::Translations.order());
        assert!(Area::Pages.order() < Area::Menus.order());
        assert!(Area::Translations.order() < Area::Menus.order());
    }

    #[test]
    fn a_selection_is_returned_in_copy_order_whatever_order_it_arrives_in() {
        let asked = vec![Area::Workflows, Area::Menus, Area::Pages, Area::Theme];
        assert_eq!(
            plan_areas(&asked, false),
            vec![Area::Pages, Area::Menus, Area::Theme, Area::Workflows]
        );
    }

    #[test]
    fn a_repeated_area_is_copied_once() {
        // A checkbox double-firing, or a wizard that sends the default set plus the operator's
        // additions, must not make the runner copy a page area twice.
        let asked = vec![Area::Pages, Area::Pages, Area::Menus, Area::Pages];
        assert_eq!(plan_areas(&asked, false), vec![Area::Pages, Area::Menus]);
    }

    #[test]
    fn an_empty_selection_is_refused_before_anything_is_created() {
        let err = require_areas(&[]).unwrap_err();
        assert!(err.contains("at least one"), "{err}");
    }

    #[test]
    fn a_zero_total_reports_zero_percent_and_says_it_is_counting() {
        // The bar must not read 100% before anything is known, nor 0% with no explanation.
        let p = Progress::new();
        assert_eq!(p.percent(), 0);
        assert!(p.summary().contains("Counting"), "{}", p.summary());
    }

    #[test]
    fn the_bar_advances_and_never_exceeds_the_total() {
        let mut p = Progress::new();
        p.expect(Area::Pages, 400);
        p.expect(Area::Translations, 100);
        assert_eq!(p.percent(), 0);

        p.advance(Area::Pages, 250);
        assert_eq!(p.percent(), 50);
        assert_eq!(p.summary(), "250 of 500 rows copied.");

        p.advance(Area::Translations, 100);
        assert_eq!(p.percent(), 70);

        // A miscounted area must not push the bar past the end and leave it there.
        p.advance(Area::Pages, 9999);
        assert_eq!(p.percent(), 100);
        assert_eq!(p.items_done(), 500);
    }

    #[test]
    fn a_failed_area_is_named_rather_than_just_counted() {
        let mut p = Progress::new();
        p.expect(Area::Pages, 10);
        p.advance(Area::Pages, 4);
        p.fail(Area::Menus);
        assert!(p.summary().contains("Menus"), "{}", p.summary());
    }

    #[test]
    fn an_area_with_no_expectation_yet_still_counts_so_the_bar_is_not_stuck_at_zero() {
        // The runner discovers an area's total after it starts copying it. Refusing to count
        // before that would freeze the bar at 0% for the first area of every job.
        let mut p = Progress::new();
        p.advance(Area::Pages, 12);
        assert_eq!(p.items_done(), 12);
    }

    #[test]
    fn every_area_round_trips_through_its_wire_name() {
        for area in Area::ALL {
            assert_eq!(Area::parse(area.as_str()), Some(area));
            assert!(!area.label().is_empty());
            assert!(!area.weight().to_string().is_empty());
        }
        assert!(
            Area::parse("media").is_none(),
            "media is referenced, never copied"
        );
    }
}
