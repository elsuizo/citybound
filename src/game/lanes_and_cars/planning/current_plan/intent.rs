use compact::{CVec, CDict};
use descartes::{N, V2, P2, Segment, Norm, FiniteCurve, Curve, RelativeToBasis,
                WithUniqueOrthogonal, RoughlyComparable, Dot};

use super::{PlanStep, Settings, LaneStrokeRef, SelectableStrokeRef, ContinuationMode};
use super::super::plan::{PlanDelta, BuiltStrokes};
use super::super::lane_stroke::{LaneStroke, LaneStrokeNode};
use itertools::Itertools;

const LANE_DISTANCE: N = 5.0;
const CENTER_LANE_DISTANCE: N = 6.0;

#[derive(Compact, Clone)]
pub enum Intent {
    None,
    NewRoad(CVec<P2>),
    ContinueRoad(CVec<(LaneStrokeRef, ContinuationMode)>, CVec<P2>, P2),
    Select(SelectableStrokeRef, N, N),
    MaximizeSelection,
    MoveSelection(V2),
    DeleteSelection,
    CreateNextLane,
}

impl Default for Intent {
    fn default() -> Self {
        Intent::None
    }
}

pub fn apply_intent(current: &PlanStep,
                    maybe_still_built_strokes: Option<&BuiltStrokes>,
                    settings: &Settings)
                    -> PlanStep {

    let still_built_strokes = || maybe_still_built_strokes.expect("still built strokes needed");

    match current.intent {
        Intent::None => current.clone(),

        Intent::NewRoad(ref points) => apply_new_road(points, current, settings),

        Intent::ContinueRoad(ref continue_from, ref additional_points, start_reference_point) => {
            apply_continue_road(continue_from,
                                additional_points,
                                start_reference_point,
                                current)
        }

        Intent::Select(selection_ref, start, end) => {
            apply_select(selection_ref,
                         start,
                         end,
                         current,
                         still_built_strokes(),
                         settings)
        }

        Intent::MaximizeSelection => apply_maximize_selection(current, still_built_strokes()),

        Intent::MoveSelection(delta) => apply_move_selection(delta, current, still_built_strokes()),

        Intent::DeleteSelection => apply_delete_selection(current, still_built_strokes()),

        Intent::CreateNextLane => apply_create_next_lane(current, still_built_strokes()),
    }
}

fn apply_new_road(points: &CVec<P2>, current: &PlanStep, settings: &Settings) -> PlanStep {
    // drawing a new road is equivalent to continuing a road
    // that consists of only its start points
    let mut one_point_strokes = CVec::<LaneStroke>::new();
    let mut continue_from = CVec::new();
    let base_idx = current.plan_delta.new_strokes.len();
    let direction = (points[1] - points[0]).normalize();
    let n_per_side = settings.n_lanes_per_side;
    let offset = |lane_idx: usize| {
        direction.orthogonal() * (CENTER_LANE_DISTANCE / 2.0 + LANE_DISTANCE * lane_idx as N)
    };

    for lane_idx in 0..n_per_side {
        one_point_strokes.push(LaneStroke::with_single_node(LaneStrokeNode {
            position: points[0] + offset(lane_idx),
            direction: direction,
        }));
        continue_from.push((LaneStrokeRef(base_idx + lane_idx), ContinuationMode::Append));
    }

    if settings.create_both_sides {
        for lane_idx in 0..n_per_side {
            one_point_strokes.push(LaneStroke::with_single_node(LaneStrokeNode {
                position: points[0] - offset(lane_idx),
                direction: -direction,
            }));
            continue_from.push((LaneStrokeRef(base_idx + lane_idx + n_per_side),
                                        ContinuationMode::Prepend));
        }
    }

    let mut new_new_strokes = current.plan_delta.new_strokes.clone();
    new_new_strokes.extend(one_point_strokes);

    let current_with_new_strokes = PlanStep {
        plan_delta: PlanDelta { new_strokes: new_new_strokes, ..current.plan_delta.clone() },
        ..current.clone()
    };

    apply_continue_road(&continue_from,
                        &points[1..],
                        points[0],
                        &current_with_new_strokes)
}

fn apply_continue_road(continue_from: &CVec<(LaneStrokeRef, ContinuationMode)>,
                       additional_points: &[P2],
                       start_reference_point: P2,
                       current: &PlanStep)
                       -> PlanStep {
    let mut previous_reference_point = start_reference_point;
    let mut new_plan_delta = current.plan_delta.clone();

    for next_reference_point in additional_points {
        // TODO: not really nice that we have to care about that here...
        if next_reference_point.is_roughly_within(previous_reference_point, ::descartes::MIN_START_TO_END) {
                    continue;
                }

        for &(LaneStrokeRef(stroke_idx), mode) in continue_from {
            let stroke = &mut new_plan_delta.new_strokes[stroke_idx];
            let (previous_position, previous_direction, next_direction) = match mode {
                ContinuationMode::Append => {
                    let node = stroke.nodes().last().unwrap();
                    (node.position,
                     node.direction,
                     Segment::arc_with_direction(previous_reference_point,
                                                 node.direction,
                                                 *next_reference_point)
                         .end_direction())

                }
                ContinuationMode::Prepend => {
                    let node = stroke.nodes()[0];
                    (node.position,
                     node.direction,
                     -Segment::arc_with_direction(previous_reference_point,
                                                  -node.direction,
                                                  *next_reference_point)
                         .end_direction())
                }
            };
            let next_position = *next_reference_point +
                                (previous_position - previous_reference_point)
                .to_basis(previous_direction)
                .from_basis(next_direction);

            let next_node = LaneStrokeNode {
                position: next_position,
                direction: next_direction,
            };

            match mode {
                ContinuationMode::Append => {
                    stroke.nodes_mut().push(next_node);
                    if !stroke.well_formed() {
                        stroke.nodes_mut().pop();
                    }
                }
                ContinuationMode::Prepend => {
                    stroke.nodes_mut().insert(0, next_node);
                    if !stroke.well_formed() {
                        stroke.nodes_mut().remove(0);
                    }
                }
            }
        }

        previous_reference_point = *next_reference_point;
    }

    PlanStep {
        plan_delta: new_plan_delta,
        selections: CDict::new(),
        intent: Intent::None,
    }
}

fn apply_select(selection_ref: SelectableStrokeRef,
                start: N,
                end: N,
                current: &PlanStep,
                still_built_strokes: &BuiltStrokes,
                settings: &Settings)
                -> PlanStep {
    let mut new_selections = current.selections.clone();
    new_selections.insert(selection_ref, (start, end));
    if settings.select_parallel {
        let stroke = selection_ref.get_stroke(&current.plan_delta, still_built_strokes);

        let start_position = stroke.path().along(start);
        let start_direction = stroke.path().direction_along(start);
        let end_position = stroke.path().along(end);
        let end_direction = stroke.path().direction_along(end);

        let mut additional_selections = Vec::new();

        let all_strokes = current.plan_delta
            .new_strokes
            .iter()
            .enumerate()
            .map(|(new_idx, new_stroke)| (SelectableStrokeRef::New(new_idx), new_stroke))
            .chain(still_built_strokes.mapping
                .pairs()
                .map(|(old_ref, old_stroke)| (SelectableStrokeRef::Built(*old_ref), old_stroke)));

        for (other_ref, other_stroke) in all_strokes {
            if other_ref != selection_ref {
                if let (Some(start_on_other_distance), Some(end_on_other_distance)) =
                    (other_stroke.path().project(start_position),
                     other_stroke.path().project(end_position)) {
                    let start_on_other = other_stroke.path()
                        .along(start_on_other_distance);
                    let start_direction_on_other = other_stroke.path()
                        .direction_along(start_on_other_distance);
                    let end_on_other = other_stroke.path().along(end_on_other_distance);
                    let end_direction_on_other = other_stroke.path()
                        .direction_along(end_on_other_distance);

                    let add_selection = start_on_other.is_roughly_within(start_position, 60.0) &&
                                        end_on_other.is_roughly_within(end_position, 60.0) &&
                                        if start_on_other_distance < end_on_other_distance {
                        start_direction_on_other.is_roughly_within(start_direction, 0.1) &&
                        end_direction_on_other.is_roughly_within(end_direction, 0.1)
                    } else if settings.select_opposite {
                        start_direction_on_other.is_roughly_within(-start_direction, 0.1) &&
                        end_direction_on_other.is_roughly_within(-end_direction, 0.1)
                    } else {
                        false
                    };
                    if add_selection {
                        additional_selections.push((other_ref,
                                   (start_on_other_distance.min(end_on_other_distance),
                                    end_on_other_distance.max(start_on_other_distance))));
                    }
                }
            }
        }

        for (other_ref, (start, end)) in additional_selections {
            new_selections.insert(other_ref, (start, end));
        }
    }

    PlanStep {
        selections: new_selections,
        intent: Intent::None,
        ..current.clone()
    }
}

fn apply_maximize_selection(current: &PlanStep, still_built_strokes: &BuiltStrokes) -> PlanStep {
    let new_selections = current.selections
        .pairs()
        .map(|(selection_ref, _)| {
            let stroke = selection_ref.get_stroke(&current.plan_delta, still_built_strokes);
            (*selection_ref, (0.0, stroke.path().length()))
        })
        .collect();
    PlanStep { selections: new_selections, ..current.clone() }
}

fn apply_move_selection(delta: V2,
                        current: &PlanStep,
                        still_built_strokes: &BuiltStrokes)
                        -> PlanStep {

    let mut new_plan_delta = current.plan_delta.clone();

    let mut with_subsections_moved = current.selections
        .pairs()
        .map(|(&selection_ref, &(start, end))| {
            let stroke = selection_ref.get_stroke(&current.plan_delta, still_built_strokes);
            (selection_ref, stroke.with_subsection_moved(start, end, delta))
        })
        .collect::<::fnv::FnvHashMap<_, _>>();

    #[derive(PartialEq, Eq)]
    enum C {
        Before,
        After,
    };

    let mut connector_alignments =
        Vec::<((SelectableStrokeRef, C), (SelectableStrokeRef, C))>::new();

    fn a_close_and_right_of_b(maybe_node_a: Option<&LaneStrokeNode>,
                              maybe_node_b: Option<&LaneStrokeNode>)
                              -> bool {
        if let (Some(node_a), Some(node_b)) = (maybe_node_a, maybe_node_b) {
            node_a.position.is_roughly_within(node_b.position, 7.0) &&
            (node_a.position - node_b.position).dot(&node_a.direction.orthogonal()) > 0.0
        } else {
            false
        }
    }

    for ((&ref_a,
          &(_,
            ref maybe_before_connector_a,
            ref new_subsection_a,
            ref maybe_after_connector_a,
            _)),
         (&ref_b,
          &(_,
            ref maybe_before_connector_b,
            ref new_subsection_b,
            ref maybe_after_connector_b,
            _))) in
        with_subsections_moved.iter()
            .cartesian_product(with_subsections_moved.iter())
            .filter(|&((a, _), (b, _))| a != b) {
        if a_close_and_right_of_b(new_subsection_a.get(0), new_subsection_b.get(0)) &&
           maybe_before_connector_a.is_some() && maybe_before_connector_b.is_some() {
            connector_alignments.push(((ref_a, C::Before), (ref_b, C::Before)));
        }
        if a_close_and_right_of_b(new_subsection_a.get(0), new_subsection_b.last()) &&
           maybe_before_connector_a.is_some() && maybe_after_connector_b.is_some() &&
           !connector_alignments.iter()
            .any(|other| other == &((ref_b, C::After), (ref_a, C::Before))) {
            connector_alignments.push(((ref_a, C::Before), (ref_b, C::After)));
        }
        if a_close_and_right_of_b(new_subsection_a.last(), new_subsection_b.last()) &&
           maybe_after_connector_a.is_some() && maybe_after_connector_b.is_some() {
            connector_alignments.push(((ref_a, C::After), (ref_b, C::After)));
        }
        if a_close_and_right_of_b(new_subsection_a.last(), new_subsection_b.get(0)) &&
           maybe_after_connector_a.is_some() && maybe_before_connector_b.is_some() &&
           !connector_alignments.iter()
            .any(|other| other == &((ref_b, C::Before), (ref_a, C::After))) {
            connector_alignments.push(((ref_a, C::After), (ref_b, C::Before)));
        }
    }

    if connector_alignments.len() > 1 {
        // figure out which alignments need to happen first
        // yes, this is not optimal at all, but correct
        while {
            let mut something_happened = false;
                        #[allow(needless_range_loop)]
            for i in 0..connector_alignments.len() {
                let swap = {
                    let &(_, ref align_a_to) = &connector_alignments[i];
                    connector_alignments.iter()
                        .position(|&(ref b, _)| align_a_to == b)
                        .and_then(|b_idx| if b_idx > i { Some(b_idx) } else { None })
                };
                if let Some(swap_with) = swap {
                    connector_alignments.swap(i, swap_with);
                    something_happened = true;
                    break;
                }
            }
            something_happened
        } {}
    }

    for ((align_ref, align_connector), (align_to_ref, align_to_connector)) in connector_alignments {
        let align_to = match align_to_connector {
            C::Before => with_subsections_moved[&align_to_ref].1.unwrap(),
            C::After => with_subsections_moved[&align_to_ref].3.unwrap(),
        };
        let align = match align_connector {
            C::Before => {
                with_subsections_moved.get_mut(&align_ref)
                    .unwrap()
                    .1
                    .as_mut()
                    .unwrap()
            }
            C::After => {
                with_subsections_moved.get_mut(&align_ref)
                    .unwrap()
                    .3
                    .as_mut()
                    .unwrap()
            }
        };

        let direction_sign = align.direction.dot(&align_to.direction).signum();
        align.direction = direction_sign * align_to.direction;
        let distance = if direction_sign < 0.0 { 6.0 } else { 5.0 };
        align.position = align_to.position + distance * align.direction.orthogonal();
    }

    let mut new_selections = CDict::new();

    for (selection_ref, (b, bc, s, ac, a)) in with_subsections_moved {
        if let Ok(new_stroke) = LaneStroke::new(b.into_iter()
                .chain(bc)
                .chain(s.clone())
                .chain(ac)
                .chain(a)
                .collect())
            .map_err(|e| println!("{:?}", e)) {
            let new_selection_start = new_stroke.path().project(s[0].position).unwrap();
            let new_selection_end = new_stroke.path().project(s.last().unwrap().position).unwrap();

            let new_selection_ref = match selection_ref {
                SelectableStrokeRef::New(stroke_idx) => {
                    new_plan_delta.new_strokes[stroke_idx] = new_stroke;
                    SelectableStrokeRef::New(stroke_idx)
                }
                SelectableStrokeRef::Built(old_ref) => {
                    let old_stroke = still_built_strokes.mapping
                        .get(old_ref)
                        .unwrap();
                    new_plan_delta.strokes_to_destroy
                        .insert(old_ref, old_stroke.clone());
                    new_plan_delta.new_strokes.push(new_stroke);
                    SelectableStrokeRef::New(new_plan_delta.new_strokes.len() - 1)
                }
            };

            new_selections.insert(new_selection_ref, (new_selection_start, new_selection_end));
        }
    }

    PlanStep {
        plan_delta: new_plan_delta,
        selections: new_selections,
        ..current.clone()
    }
}

fn apply_delete_selection(current: &PlanStep, still_built_strokes: &BuiltStrokes) -> PlanStep {
    let mut new_plan_delta = current.plan_delta.clone();
    let mut new_stroke_indices_to_remove = Vec::new();
    let mut new_strokes = Vec::new();

    for (&selection_ref, &(start, end)) in current.selections.pairs() {
        let stroke = selection_ref.get_stroke(&current.plan_delta, still_built_strokes);
        if let Some(before) = stroke.subsection(0.0, start) {
            new_strokes.push(before);
        }
        if let Some(after) = stroke.subsection(end, stroke.path().length()) {
            new_strokes.push(after);
        }
    }

    new_stroke_indices_to_remove.sort();

    for index_to_remove in new_stroke_indices_to_remove.into_iter().rev() {
        new_plan_delta.new_strokes.remove(index_to_remove);
    }

    for new_stroke in new_strokes {
        new_plan_delta.new_strokes.push(new_stroke);
    }

    PlanStep {
        plan_delta: new_plan_delta,
        selections: CDict::new(),
        intent: Intent::None,
    }
}

fn apply_create_next_lane(current: &PlanStep, still_built_strokes: &BuiltStrokes) -> PlanStep {
    let selected_subsections = current.selections
        .pairs()
        .filter_map(|(&selection_ref, &(start, end))| {
            let stroke = selection_ref.get_stroke(&current.plan_delta, still_built_strokes);
            stroke.subsection(start, end)
        })
        .collect::<Vec<_>>();
    let next_lane_strokes = selected_subsections.iter()
        .filter_map(|stroke| {
            let offset_nodes = stroke.nodes()
                .iter()
                .map(|node| {
                    LaneStrokeNode {
                        position: node.position + node.direction.orthogonal() * LANE_DISTANCE,
                        direction: node.direction,
                    }
                })
                .collect();
            LaneStroke::new(offset_nodes).ok()
        })
        .filter(|stroke| {
            !selected_subsections.iter().any(|subsection| stroke.is_roughly_within(subsection, 0.1))
        });
    let mut new_new_strokes = current.plan_delta.new_strokes.clone();
    new_new_strokes.extend(next_lane_strokes);

    PlanStep {
        plan_delta: PlanDelta { new_strokes: new_new_strokes, ..current.plan_delta.clone() },
        selections: CDict::new(),
        intent: Intent::None,
    }
}