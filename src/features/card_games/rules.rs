//! 出牌牌型识别与压制规则。
//!
//! 从 `card_games.rs` 拆出：纯函数式规则判定（顺子/连对/飞机/炸弹…），
//! 与对局状态机解耦，便于单独覆盖边界牌型。

use super::*;

pub(super) fn parse_cards(text: &str) -> Result<Vec<Rank>, String> {
    let compact = text
        .chars()
        .filter(|ch| !ch.is_whitespace() && !matches!(ch, ',' | '，' | '、'))
        .collect::<String>();
    let mut rest = compact.as_str();
    let mut ranks = Vec::new();
    while !rest.is_empty() {
        let (rank, consumed) = if rest.starts_with("小王") {
            (Rank::SmallJoker, "小王".len())
        } else if rest.starts_with("大王") {
            (Rank::BigJoker, "大王".len())
        } else if rest.starts_with("10") {
            (Rank::Ten, 2)
        } else {
            let ch = rest.chars().next().expect("rest is not empty");
            let rank = match ch.to_ascii_uppercase() {
                '3' => Rank::Three,
                '4' => Rank::Four,
                '5' => Rank::Five,
                '6' => Rank::Six,
                '7' => Rank::Seven,
                '8' => Rank::Eight,
                '9' => Rank::Nine,
                'J' => Rank::Jack,
                'Q' => Rank::Queen,
                'K' => Rank::King,
                'A' => Rank::Ace,
                '2' => Rank::Two,
                _ => {
                    return Err(reply(
                        &templates::PARSE_UNKNOWN_CARD,
                        ReplyVars::new().with("牌面", ch.to_string()),
                    ));
                }
            };
            (rank, ch.len_utf8())
        };
        ranks.push(rank);
        rest = &rest[consumed..];
    }
    ranks.sort_unstable();
    Ok(ranks)
}

pub(super) fn take_cards(hand: &[Card], ranks: &[Rank]) -> Result<Vec<Card>, String> {
    let mut available = hand.to_vec();
    let mut cards = Vec::with_capacity(ranks.len());
    for rank in ranks {
        let index = available
            .iter()
            .position(|card| card.rank == *rank && !card.spade_three)
            .or_else(|| available.iter().position(|card| card.rank == *rank))
            .ok_or_else(|| {
                reply(
                    &templates::PARSE_MISSING_CARD,
                    ReplyVars::new().with("牌面", rank.label()),
                )
            })?;
        cards.push(available.remove(index));
    }
    Ok(cards)
}

pub(super) fn take_cards_for_play(
    hand: &[Card],
    ranks: &[Rank],
    requires_spade_three: bool,
) -> Result<Vec<Card>, String> {
    let mut cards = take_cards(hand, ranks)?;
    if requires_spade_three
        && ranks.contains(&Rank::Three)
        && !cards.iter().any(|card| card.spade_three)
        && let Some(spade_three) = hand.iter().find(|card| card.spade_three).copied()
        && let Some(normal_three) = cards.iter_mut().find(|card| card.rank == Rank::Three)
    {
        *normal_three = spade_three;
    }
    Ok(cards)
}

pub(super) fn remove_cards(hand: &mut Vec<Card>, cards: &[Card]) {
    for card in cards {
        let index = hand
            .iter()
            .position(|candidate| candidate == card)
            .expect("validated card exists in hand");
        hand.remove(index);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlayPattern {
    Single(Rank),
    Pair(Rank),
    Triple(Rank),
    TripleSingle(Rank),
    TriplePair(Rank),
    Straight { high: Rank, length: usize },
    PairStraight { high: Rank, length: usize },
    Airplane { high: Rank, triples: usize },
    AirplaneSingles { high: Rank, triples: usize },
    AirplanePairs { high: Rank, triples: usize },
    FourTwoSingles(Rank),
    FourTwoPairs(Rank),
    Bomb(Rank),
    JokerBomb,
}

impl PlayPattern {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Single(_) => "单张",
            Self::Pair(_) => "对子",
            Self::Triple(_) => "三张",
            Self::TripleSingle(_) => "三带一",
            Self::TriplePair(_) => "三带二",
            Self::Straight { .. } => "顺子",
            Self::PairStraight { .. } => "连对",
            Self::Airplane { .. } => "飞机",
            Self::AirplaneSingles { .. } => "飞机带单",
            Self::AirplanePairs { .. } => "飞机带对",
            Self::FourTwoSingles(_) => "四带二单",
            Self::FourTwoPairs(_) => "四带二对",
            Self::Bomb(_) => "炸弹",
            Self::JokerBomb => "王炸",
        }
    }

    pub(super) fn is_bomb(&self) -> bool {
        matches!(self, Self::Bomb(_) | Self::JokerBomb)
    }

    pub(super) fn strength(&self) -> u8 {
        match self {
            Self::Single(rank)
            | Self::Pair(rank)
            | Self::Triple(rank)
            | Self::TripleSingle(rank)
            | Self::TriplePair(rank)
            | Self::FourTwoSingles(rank)
            | Self::FourTwoPairs(rank)
            | Self::Bomb(rank) => *rank as u8,
            Self::Straight { high, .. }
            | Self::PairStraight { high, .. }
            | Self::Airplane { high, .. }
            | Self::AirplaneSingles { high, .. }
            | Self::AirplanePairs { high, .. } => *high as u8,
            Self::JokerBomb => u8::MAX,
        }
    }

    pub(super) fn beats(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::JokerBomb, _) => !matches!(previous, Self::JokerBomb),
            (_, Self::JokerBomb) => false,
            (Self::Bomb(left), Self::Bomb(right)) => left > right,
            (Self::Bomb(_), _) => true,
            (_, Self::Bomb(_)) => false,
            (Self::Single(left), Self::Single(right))
            | (Self::Pair(left), Self::Pair(right))
            | (Self::Triple(left), Self::Triple(right))
            | (Self::TripleSingle(left), Self::TripleSingle(right))
            | (Self::TriplePair(left), Self::TriplePair(right))
            | (Self::FourTwoSingles(left), Self::FourTwoSingles(right))
            | (Self::FourTwoPairs(left), Self::FourTwoPairs(right)) => left > right,
            (
                Self::Straight {
                    high: left,
                    length: left_len,
                },
                Self::Straight {
                    high: right,
                    length: right_len,
                },
            )
            | (
                Self::PairStraight {
                    high: left,
                    length: left_len,
                },
                Self::PairStraight {
                    high: right,
                    length: right_len,
                },
            ) => left_len == right_len && left > right,
            (
                Self::Airplane {
                    high: left,
                    triples: left_len,
                },
                Self::Airplane {
                    high: right,
                    triples: right_len,
                },
            )
            | (
                Self::AirplaneSingles {
                    high: left,
                    triples: left_len,
                },
                Self::AirplaneSingles {
                    high: right,
                    triples: right_len,
                },
            )
            | (
                Self::AirplanePairs {
                    high: left,
                    triples: left_len,
                },
                Self::AirplanePairs {
                    high: right,
                    triples: right_len,
                },
            ) => left_len == right_len && left > right,
            _ => false,
        }
    }
}

pub(super) fn classify(ranks: &[Rank]) -> Option<PlayPattern> {
    let counts = rank_counts(ranks);
    let len = ranks.len();
    if len == 1 {
        return Some(PlayPattern::Single(ranks[0]));
    }
    if len == 2 {
        if counts.len() == 2
            && counts.contains_key(&Rank::SmallJoker)
            && counts.contains_key(&Rank::BigJoker)
        {
            return Some(PlayPattern::JokerBomb);
        }
        return single_group(&counts, 2).map(PlayPattern::Pair);
    }
    if len == 3 {
        return single_group(&counts, 3).map(PlayPattern::Triple);
    }
    if len == 4 {
        if let Some(rank) = single_group(&counts, 4) {
            return Some(PlayPattern::Bomb(rank));
        }
        if let Some(rank) = group_rank(&counts, 3) {
            return Some(PlayPattern::TripleSingle(rank));
        }
    }
    if len == 5
        && let Some(rank) = group_rank(&counts, 3)
        && counts.values().any(|count| *count == 2)
    {
        return Some(PlayPattern::TriplePair(rank));
    }
    if len >= 5 && counts.values().all(|count| *count == 1) && consecutive(&counts, 1) {
        return Some(PlayPattern::Straight {
            high: *counts.keys().next_back()?,
            length: counts.len(),
        });
    }
    if len >= 6
        && len.is_multiple_of(2)
        && counts.values().all(|count| *count == 2)
        && consecutive(&counts, 2)
    {
        return Some(PlayPattern::PairStraight {
            high: *counts.keys().next_back()?,
            length: counts.len(),
        });
    }
    if len == 6
        && let Some(rank) = group_rank(&counts, 4)
    {
        return Some(PlayPattern::FourTwoSingles(rank));
    }
    if len == 8
        && let Some(rank) = group_rank(&counts, 4)
    {
        let pairs = counts
            .iter()
            .filter(|(candidate, count)| **candidate != rank && **count == 2)
            .count();
        if pairs == 2 {
            return Some(PlayPattern::FourTwoPairs(rank));
        }
    }
    classify_airplane(&counts, len)
}

pub(super) fn classify_for_variant(ranks: &[Rank], variant: CardGameVariant) -> Option<PlayPattern> {
    if variant == CardGameVariant::HunanRunFast {
        let counts = rank_counts(ranks);
        if ranks.len() == 4 && counts.values().all(|count| *count == 2) && consecutive(&counts, 2) {
            return Some(PlayPattern::PairStraight {
                high: *counts.keys().next_back()?,
                length: counts.len(),
            });
        }
    }
    classify(ranks)
}

pub(super) fn lowest_beating_play(hand: &[Card], previous: &PlayPattern) -> Option<(Vec<Card>, PlayPattern)> {
    lowest_beating_play_for_variant(hand, previous, CardGameVariant::Landlord)
}

pub(super) fn lowest_beating_play_for_variant(
    hand: &[Card],
    previous: &PlayPattern,
    variant: CardGameVariant,
) -> Option<(Vec<Card>, PlayPattern)> {
    let counts = rank_counts(&hand.iter().map(|card| card.rank).collect::<Vec<_>>());
    let groups = counts.into_iter().collect::<Vec<_>>();
    let mut selected = Vec::new();
    let mut best: Option<(Vec<Rank>, PlayPattern)> = None;
    collect_beating_plays(&groups, 0, &mut selected, previous, variant, &mut best);
    let (ranks, pattern) = best?;
    let cards = take_cards(hand, &ranks).ok()?;
    Some((cards, pattern))
}

pub(super) struct RunFastPlayFilter<'a> {
    pub(super) previous: Option<&'a PlayPattern>,
    pub(super) requires_spade_three: bool,
    pub(super) required_single: Option<Rank>,
}

pub(super) fn collect_run_fast_legal_plays(
    groups: &[(Rank, usize)],
    index: usize,
    selected: &mut Vec<Rank>,
    filter: &RunFastPlayFilter<'_>,
    options: &mut Vec<(Vec<Rank>, PlayPattern)>,
) {
    if options.len() >= 2 {
        return;
    }
    if index == groups.len() {
        let Some(pattern) = classify_for_variant(selected, CardGameVariant::HunanRunFast) else {
            return;
        };
        if filter.requires_spade_three && !selected.contains(&Rank::Three) {
            return;
        }
        if let Some(previous) = filter.previous
            && !pattern.beats(previous)
        {
            return;
        }
        if let (PlayPattern::Single(rank), Some(required)) = (&pattern, filter.required_single)
            && *rank != required
        {
            return;
        }
        options.push((selected.clone(), pattern));
        return;
    }

    let (rank, count) = groups[index];
    for take in 0..=count {
        selected.extend(std::iter::repeat_n(rank, take));
        collect_run_fast_legal_plays(groups, index + 1, selected, filter, options);
        selected.truncate(selected.len() - take);
        if options.len() >= 2 {
            break;
        }
    }
}

pub(super) fn collect_beating_plays(
    groups: &[(Rank, usize)],
    index: usize,
    selected: &mut Vec<Rank>,
    previous: &PlayPattern,
    variant: CardGameVariant,
    best: &mut Option<(Vec<Rank>, PlayPattern)>,
) {
    if index == groups.len() {
        let Some(pattern) = classify_for_variant(selected, variant) else {
            return;
        };
        if !pattern.beats(previous) {
            return;
        }
        let score = (
            u8::from(pattern.is_bomb()),
            pattern.strength(),
            selected.len(),
        );
        let replace = best.as_ref().is_none_or(|(current_ranks, current)| {
            score
                < (
                    u8::from(current.is_bomb()),
                    current.strength(),
                    current_ranks.len(),
                )
        });
        if replace {
            *best = Some((selected.clone(), pattern));
        }
        return;
    }

    let (rank, count) = groups[index];
    for take in 0..=count {
        selected.extend(std::iter::repeat_n(rank, take));
        collect_beating_plays(groups, index + 1, selected, previous, variant, best);
        selected.truncate(selected.len() - take);
    }
}

pub(super) fn classify_airplane(counts: &BTreeMap<Rank, usize>, len: usize) -> Option<PlayPattern> {
    for (unit, kind) in [(3usize, 0u8), (4, 1), (5, 2)] {
        if !len.is_multiple_of(unit) {
            continue;
        }
        let triples = len / unit;
        if triples < 2 {
            continue;
        }
        for start in Rank::Three as u8..=Rank::Ace as u8 {
            let end = start + triples as u8 - 1;
            if end > Rank::Ace as u8 {
                break;
            }
            let body = (start..=end)
                .map(rank_from_value)
                .collect::<Option<Vec<_>>>()?;
            if !body.iter().all(|rank| counts.get(rank).copied() == Some(3)) {
                continue;
            }
            let wings = counts
                .iter()
                .filter(|(rank, _)| !body.contains(rank))
                .map(|(rank, count)| (*rank, *count))
                .collect::<Vec<_>>();
            let valid = match kind {
                0 => wings.is_empty(),
                1 => {
                    wings.iter().map(|(_, count)| count).sum::<usize>() == triples
                        && wings.iter().all(|(_, count)| *count <= 2)
                }
                2 => wings.len() == triples && wings.iter().all(|(_, count)| *count == 2),
                _ => unreachable!(),
            };
            if valid {
                let high = *body.last()?;
                return Some(match kind {
                    0 => PlayPattern::Airplane { high, triples },
                    1 => PlayPattern::AirplaneSingles { high, triples },
                    2 => PlayPattern::AirplanePairs { high, triples },
                    _ => unreachable!(),
                });
            }
        }
    }
    None
}

pub(super) fn rank_counts(ranks: &[Rank]) -> BTreeMap<Rank, usize> {
    let mut counts = BTreeMap::new();
    for rank in ranks {
        *counts.entry(*rank).or_insert(0) += 1;
    }
    counts
}

pub(super) fn single_group(counts: &BTreeMap<Rank, usize>, size: usize) -> Option<Rank> {
    (counts.len() == 1)
        .then(|| counts.iter().next())
        .flatten()
        .filter(|(_, count)| **count == size)
        .map(|(rank, _)| *rank)
}

pub(super) fn group_rank(counts: &BTreeMap<Rank, usize>, size: usize) -> Option<Rank> {
    counts
        .iter()
        .find(|(_, count)| **count == size)
        .map(|(rank, _)| *rank)
}

pub(super) fn consecutive(counts: &BTreeMap<Rank, usize>, expected_count: usize) -> bool {
    if counts.values().any(|count| *count != expected_count) {
        return false;
    }
    let Some(first) = counts.keys().next().and_then(|rank| rank.sequence_value()) else {
        return false;
    };
    counts
        .keys()
        .enumerate()
        .all(|(index, rank)| rank.sequence_value() == Some(first.saturating_add(index as u8)))
}
