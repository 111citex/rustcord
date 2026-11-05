//! Message polls: what Discord sends, and working out what they say.

use serde::Deserialize;

use crate::model::ReactionEmoji;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct PollMedia {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub emoji: Option<ReactionEmoji>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct PollAnswer {
    pub answer_id: u32,
    #[serde(default)]
    pub poll_media: PollMedia,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct AnswerCount {
    pub id: u32,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub me_voted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct PollResults {
    #[serde(default)]
    pub is_finalized: bool,
    #[serde(default)]
    pub answer_counts: Vec<AnswerCount>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Poll {
    #[serde(default)]
    pub question: PollMedia,
    #[serde(default)]
    pub answers: Vec<PollAnswer>,
    /// ISO 8601; none for a poll that never ends by itself.
    #[serde(default)]
    pub expiry: Option<String>,
    #[serde(default)]
    pub allow_multiselect: bool,
    #[serde(default)]
    pub results: Option<PollResults>,
}

impl Poll {
    fn answer_count(&self, answer: u32) -> Option<&AnswerCount> {
        self.results.as_ref()?.answer_counts.iter().find(|c| c.id == answer)
    }

    pub fn count(&self, answer: u32) -> u32 {
        self.answer_count(answer).map_or(0, |c| c.count)
    }

    pub fn me_voted(&self, answer: u32) -> bool {
        self.answer_count(answer).is_some_and(|c| c.me_voted)
    }

    /// Whether we've voted for anything.
    pub fn voted(&self) -> bool {
        self.my_votes().next().is_some()
    }

    fn my_votes(&self) -> impl Iterator<Item = u32> + '_ {
        self.results.iter().flat_map(|r| &r.answer_counts).filter(|c| c.me_voted).map(|c| c.id)
    }

    pub fn total_votes(&self) -> u32 {
        self.results.iter().flat_map(|r| &r.answer_counts).map(|c| c.count).sum()
    }

    /// `answer`'s share of the votes, as a whole percentage.
    pub fn percent(&self, answer: u32) -> u32 {
        match self.total_votes() {
            0 => 0,
            total => (f64::from(self.count(answer)) * 100. / f64::from(total)).round() as u32,
        }
    }

    pub fn is_closed(&self, now_ms: u64) -> bool {
        self.results.as_ref().is_some_and(|r| r.is_finalized) || self.ends_at().is_some_and(|end| end <= now_ms)
    }

    fn ends_at(&self) -> Option<u64> {
        let end = chrono::DateTime::parse_from_rfc3339(self.expiry.as_deref()?).ok()?;
        Some(end.timestamp_millis().max(0) as u64)
    }

    /// `answer` won: the poll is over and nothing got more votes.
    pub fn is_winner(&self, answer: u32, now_ms: u64) -> bool {
        let most = self.answers.iter().map(|a| self.count(a.answer_id)).max().unwrap_or(0);
        self.is_closed(now_ms) && most > 0 && self.count(answer) == most
    }

    /// "12h left", for the poll's footer.
    pub fn time_left(&self, now_ms: u64) -> Option<String> {
        let left = self.ends_at()?.checked_sub(now_ms)? / 1000;
        Some(match left {
            0..60 => "Less than a minute left".to_string(),
            60..3_600 => format!("{}m left", left / 60),
            3_600..86_400 => format!("{}h left", left / 3_600),
            _ => format!("{}d left", left / 86_400),
        })
    }

    /// A vote someone cast or took back. Our own come back to us after we've
    /// already counted them, so those only change anything once.
    pub fn apply_vote(&mut self, answer: u32, mine: bool, add: bool) {
        if mine && self.me_voted(answer) == add {
            return;
        }
        let results = self.results.get_or_insert_with(PollResults::default);
        let at = match results.answer_counts.iter().position(|c| c.id == answer) {
            Some(at) => at,
            None => {
                results.answer_counts.push(AnswerCount { id: answer, count: 0, me_voted: false });
                results.answer_counts.len() - 1
            }
        };
        let entry = &mut results.answer_counts[at];
        entry.count = if add { entry.count + 1 } else { entry.count.saturating_sub(1) };
        if mine {
            entry.me_voted = add;
        }
    }

    /// Makes `answers` our votes; returns what they were.
    pub fn set_my_votes(&mut self, answers: &[u32]) -> Vec<u32> {
        let before: Vec<u32> = self.my_votes().collect();
        for old in before.iter().filter(|a| !answers.contains(a)) {
            self.apply_vote(*old, true, false);
        }
        for new in answers {
            self.apply_vote(*new, true, true);
        }
        before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll() -> Poll {
        serde_json::from_value(serde_json::json!({
            "question": {"text": "Demote niz"},
            "answers": [
                {"answer_id": 1, "poll_media": {"text": "Yes"}},
                {"answer_id": 2, "poll_media": {"text": "No", "emoji": {"id": null, "name": "x"}}},
            ],
            "expiry": "2026-11-01T19:19:00+00:00",
            "allow_multiselect": false,
            "results": {"is_finalized": true, "answer_counts": [
                {"id": 1, "count": 4, "me_voted": false},
                {"id": 2, "count": 5, "me_voted": true},
            ]},
        }))
        .unwrap()
    }

    #[test]
    fn reads_what_discord_sends_and_adds_it_up() {
        let p = poll();
        assert_eq!(p.question.text.as_deref(), Some("Demote niz"));
        assert_eq!(p.total_votes(), 9);
        assert_eq!((p.percent(1), p.percent(2)), (44, 56));
        assert!(p.voted() && p.me_voted(2) && !p.me_voted(1));
        assert!(p.is_winner(2, 0) && !p.is_winner(1, 0));
    }

    #[test]
    fn closes_when_finalized_or_past_its_end() {
        let mut p = poll();
        p.results.as_mut().unwrap().is_finalized = false;
        let end = p.ends_at().unwrap();
        assert!(!p.is_closed(end - 1));
        assert!(p.is_closed(end));
        assert!(!p.is_winner(2, end - 1));
        assert_eq!(p.time_left(end - 7_200_000).as_deref(), Some("2h left"));
        assert_eq!(p.time_left(end + 1), None);
    }

    #[test]
    fn nobody_wins_with_no_votes() {
        let mut p = poll();
        p.results.as_mut().unwrap().answer_counts.clear();
        assert!(!p.is_winner(1, 0));
        assert_eq!(p.percent(1), 0);
    }

    #[test]
    fn our_own_echo_is_not_counted_twice() {
        let mut p = poll();
        p.apply_vote(2, true, true);
        assert_eq!(p.count(2), 5);
        p.apply_vote(2, false, true);
        assert_eq!(p.count(2), 6);
        p.apply_vote(2, true, false);
        assert_eq!(p.count(2), 5);
        assert!(!p.me_voted(2));
        p.apply_vote(2, true, false);
        assert_eq!(p.count(2), 5);
    }

    #[test]
    fn changing_our_vote_moves_it_and_says_what_it_was() {
        let mut p = poll();
        assert_eq!(p.set_my_votes(&[1]), vec![2]);
        assert_eq!((p.count(1), p.count(2)), (5, 4));
        assert_eq!(p.set_my_votes(&[]), vec![1]);
        assert!(!p.voted());
    }

    #[test]
    fn a_poll_with_no_results_yet_can_take_a_vote() {
        let mut p = Poll { results: None, ..poll() };
        p.apply_vote(1, false, true);
        assert_eq!(p.count(1), 1);
    }
}
