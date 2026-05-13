use super::{
    CteState, MultiStageInodeMember, MultiStageInodeMemberType, MultiStageLeafMemberType,
    MultiStageMember, MultiStageMemberQueryPlanner, MultiStageMemberType,
    MultiStageQueryDescription, RollingWindowDescription, TimeSeriesDescription,
};
use crate::cube_bridge::measure_definition::RollingWindow;
use crate::logical_plan::*;
use crate::planner::apply_static_filter_to_symbol;
use crate::planner::collectors::has_multi_stage_members;
use crate::planner::collectors::member_childs;
use crate::planner::filter::base_filter::FilterType;
use crate::planner::filter::BaseFilter;
use crate::planner::filter::FilterItem;
use crate::planner::filter::FilterOperator;
use crate::planner::query_tools::QueryTools;
use crate::planner::Case;
use crate::planner::CaseSwitchDefinition;
use crate::planner::CaseSwitchItem;
use crate::planner::GranularityHelper;
use crate::planner::MeasureKind;
use crate::planner::MemberSymbol;
use crate::planner::MultiStageFilter;
use crate::planner::MultiStageFilterMode;
use crate::planner::QueryProperties;
use cubenativeutils::CubeError;
use indexmap::IndexMap;
use itertools::Itertools;
use std::collections::HashSet;
use std::rc::Rc;

pub struct MultiStageQueryPlanner {
    query_tools: Rc<QueryTools>,
    query_properties: Rc<QueryProperties>,
    // The initial multi-stage CTE state. Shared immutably; any mutation goes
    // through `as_ref().clone()` on the consumer side. Used both as the entry
    // state for the recursive planner and as the reset target for `mode:
    // fixed` filter directives.
    root_state: Rc<QueryProperties>,
}

impl MultiStageQueryPlanner {
    pub fn try_new(
        query_tools: Rc<QueryTools>,
        query_properties: Rc<QueryProperties>,
    ) -> Result<Self, CubeError> {
        let root_state = Self::build_root_state(&query_tools, &query_properties)?;
        Ok(Self {
            query_tools,
            query_properties,
            root_state,
        })
    }

    // The CTE-side mirror of `query_properties`: same dimensions/filters/
    // segments, but `measures_filters` are intentionally dropped (CTE queries
    // do not propagate them) and `order_by` is forced to an empty vec so the
    // builder skips default_order — this value is only ever used as a state
    // container, never planned directly.
    fn build_root_state(
        query_tools: &Rc<QueryTools>,
        query_properties: &Rc<QueryProperties>,
    ) -> Result<Rc<QueryProperties>, CubeError> {
        QueryProperties::builder()
            .query_tools(query_tools.clone())
            .dimensions(query_properties.dimensions().clone())
            .time_dimensions(query_properties.time_dimensions().clone())
            .dimensions_filters(query_properties.dimensions_filters().clone())
            .time_dimensions_filters(query_properties.time_dimensions_filters().clone())
            .segments(query_properties.segments().clone())
            .order_by(Some(vec![]))
            .build()
    }

    fn root_state(&self) -> &Rc<QueryProperties> {
        &self.root_state
    }

    pub fn plan_queries(&self, cte_state: &mut CteState) -> Result<(), CubeError> {
        let multi_stage_members = self
            .query_properties
            .all_used_symbols()?
            .into_iter()
            .filter_map(|memb| -> Option<Result<_, CubeError>> {
                match has_multi_stage_members(&memb, false) {
                    Ok(true) => Some(Ok(memb)),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if multi_stage_members.is_empty() {
            return Ok(());
        }

        let mut descriptions = Vec::new();
        let state = self.root_state.clone();

        let mut resolved_multi_stage_dimensions = HashSet::new();

        for member in multi_stage_members {
            let description = self.make_queries_descriptions(
                member.clone(),
                state.clone(),
                &mut descriptions,
                &mut resolved_multi_stage_dimensions,
                cte_state,
            )?;
            if !description.is_multi_stage_dimension() {
                let result = MultiStageSubqueryRef::builder()
                    .name(description.alias().clone())
                    .symbols(vec![description.member_node().clone()])
                    .schema(description.schema().clone())
                    .build();
                cte_state.add_subquery_ref(Rc::new(result));
            }
        }

        for descr in descriptions.into_iter() {
            let planner = MultiStageMemberQueryPlanner::new(
                self.query_tools.clone(),
                self.query_properties.clone(),
                descr.clone(),
            );
            let member = planner.plan_logical_query()?;
            cte_state.add_member(member);
        }

        Ok(())
    }

    fn create_multi_stage_inode_member(
        &self,
        base_member: Rc<MemberSymbol>,
        resolved_multi_stage_dimensions: &mut HashSet<String>,
    ) -> Result<(MultiStageInodeMember, bool), CubeError> {
        let inode = if let Ok(measure) = base_member.as_measure() {
            let member_type = match measure.kind() {
                MeasureKind::Rank => MultiStageInodeMemberType::Rank,
                MeasureKind::Calculated(_) => MultiStageInodeMemberType::Calculate,
                _ => MultiStageInodeMemberType::Aggregate,
            };

            let time_shift = measure.time_shift().cloned();

            let is_ungrupped = match &member_type {
                MultiStageInodeMemberType::Rank | MultiStageInodeMemberType::Calculate => true,
                _ => self.query_properties.ungrouped(),
            };

            let reduce_by = measure.reduce_by().cloned().unwrap_or_default();
            let add_group_by = measure.add_group_by().cloned().unwrap_or_default();
            let group_by = measure.group_by().cloned();
            (
                MultiStageInodeMember::new(
                    member_type,
                    reduce_by,
                    add_group_by,
                    group_by,
                    time_shift,
                ),
                is_ungrupped,
            )
        } else {
            let add_group_by = if let Ok(dimension) = base_member.as_dimension() {
                dimension.add_group_by().cloned().unwrap_or_default()
            } else {
                vec![]
            };
            resolved_multi_stage_dimensions
                .insert(base_member.clone().resolve_reference_chain().full_name());
            (
                MultiStageInodeMember::new(
                    MultiStageInodeMemberType::Dimension,
                    vec![],
                    add_group_by,
                    None,
                    None,
                ),
                false,
            )
        };
        Ok(inode)
    }

    fn make_childs(
        &self,
        member: Rc<MemberSymbol>,
        new_state: Rc<QueryProperties>,
        result: &mut Vec<Rc<MultiStageQueryDescription>>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
        resolved_multi_stage_dimensions: &mut HashSet<String>,
        cte_state: &mut CteState,
    ) -> Result<(), CubeError> {
        if let Some(Case::CaseSwitch(case_switch)) = member.case() {
            if self.try_make_childs_for_case_switch(
                case_switch,
                new_state.clone(),
                result,
                descriptions,
                resolved_multi_stage_dimensions,
                cte_state,
            )? {
                return Ok(());
            }
        }
        self.default_make_childs(
            member,
            new_state,
            result,
            descriptions,
            resolved_multi_stage_dimensions,
            cte_state,
        )
    }

    fn is_multi_stage_dimension(member: &Rc<MemberSymbol>) -> Result<bool, CubeError> {
        if member.is_dimension() {
            has_multi_stage_members(member, false)
        } else {
            Ok(false)
        }
    }

    fn default_make_childs(
        &self,
        member: Rc<MemberSymbol>,
        new_state: Rc<QueryProperties>,
        result: &mut Vec<Rc<MultiStageQueryDescription>>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
        resolved_multi_stage_dimensions: &mut HashSet<String>,
        cte_state: &mut CteState,
    ) -> Result<(), CubeError> {
        let mut has_inputs = false;
        for dep in member.get_dependencies() {
            let dep = &dep.resolve_reference_chain();
            if dep.is_measure() || Self::is_multi_stage_dimension(dep)? {
                has_inputs = true;
                let description = self.make_queries_descriptions(
                    dep.clone(),
                    new_state.clone(),
                    descriptions,
                    resolved_multi_stage_dimensions,
                    cte_state,
                )?;
                if !description.is_multi_stage_dimension() || member.as_dimension().is_ok() {
                    result.push(description);
                }
            }
        }
        if !has_inputs {
            //Rank and similas cases

            let alias = cte_state.next_cte_name();
            let description = MultiStageQueryDescription::new(
                MultiStageMember::new_without_member_leaf(
                    MultiStageMemberType::Leaf(MultiStageLeafMemberType::Measure),
                    member.clone(),
                    self.query_properties.ungrouped(),
                    false,
                ),
                new_state.clone(),
                vec![],
                alias,
            );
            result.push(description.clone());
            descriptions.push(description.clone());
        }
        Ok(())
    }

    fn try_make_childs_for_case_switch(
        &self,
        case: &CaseSwitchDefinition,
        new_state: Rc<QueryProperties>,
        result: &mut Vec<Rc<MultiStageQueryDescription>>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
        resolved_multi_stage_dimensions: &mut HashSet<String>,
        cte_state: &mut CteState,
    ) -> Result<bool, CubeError> {
        let CaseSwitchItem::Member(switch_member) = &case.switch else {
            return Ok(false);
        };

        // Collect, per dependency, the union of switch values that need it.
        // `None` marks an unrestricted (open ELSE) entry: such a dependency
        // must be processed without a prefilter on switch_member, since the
        // outer CASE will dispatch by value at row level.
        let mut deps: IndexMap<String, (Rc<MemberSymbol>, Option<Vec<String>>)> = IndexMap::new();

        let mut record = |dep: Rc<MemberSymbol>, branch_values: Option<Vec<String>>| {
            let dep = dep.resolve_reference_chain();
            let entry = deps
                .entry(dep.full_name())
                .or_insert_with(|| (dep.clone(), Some(Vec::new())));
            match (&mut entry.1, branch_values) {
                (None, _) => {} // already unrestricted
                (slot @ Some(_), None) => *slot = None,
                (Some(values), Some(branch)) => {
                    for v in branch {
                        if !values.contains(&v) {
                            values.push(v);
                        }
                    }
                }
            }
        };

        for itm in &case.items {
            for dep in itm.sql.get_dependencies() {
                record(dep, Some(vec![itm.value.clone()]));
            }
        }

        if let Some(else_sql) = &case.else_sql {
            let else_values = case.get_else_values();
            for dep in else_sql.get_dependencies() {
                record(dep.clone(), else_values.clone());
            }
        }

        for (_, (dep, values)) in deps {
            let mut state = new_state.as_ref().clone();
            if let Some(values) = values {
                if !values.is_empty() {
                    let filter = BaseFilter::try_new(
                        self.query_tools.clone(),
                        switch_member.clone(),
                        FilterType::Dimension,
                        FilterOperator::Equal,
                        Some(values.into_iter().map(Some).collect_vec()),
                    )?;
                    state.add_dimension_filter(FilterItem::Item(filter));
                }
            }
            let state = Rc::new(state);
            result.push(self.make_queries_descriptions(
                dep,
                state,
                descriptions,
                resolved_multi_stage_dimensions,
                cte_state,
            )?);
        }

        Ok(true)
    }

    fn make_queries_descriptions(
        &self,
        member: Rc<MemberSymbol>,
        state: Rc<QueryProperties>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
        resolved_multi_stage_dimensions: &mut HashSet<String>,
        cte_state: &mut CteState,
    ) -> Result<Rc<MultiStageQueryDescription>, CubeError> {
        let member = member.resolve_reference_chain();
        let member = apply_static_filter_to_symbol(&member, state.dimensions_filters())?;
        let state = if member.is_dimension() {
            let mut new_state = state.as_ref().clone();
            new_state.remove_multistage_dimensions(resolved_multi_stage_dimensions)?;
            Rc::new(new_state)
        } else {
            state
        };

        let member_name = member.full_name();
        if let Some(exists) = descriptions
            .iter()
            .find(|q| q.is_match_member_and_state(&member, &state))
        {
            return Ok(exists.clone());
        };

        if let Some(rolling_window_query) = self.try_plan_rolling_window(
            member.clone(),
            state.clone(),
            descriptions,
            resolved_multi_stage_dimensions,
            cte_state,
        )? {
            return Ok(rolling_window_query);
        }

        let has_multi_stage_members = has_multi_stage_members(&member, false)?;
        let description = if !has_multi_stage_members {
            let alias = cte_state.next_cte_name();
            MultiStageQueryDescription::new(
                MultiStageMember::new(
                    MultiStageMemberType::Leaf(MultiStageLeafMemberType::Measure),
                    member.clone(),
                    self.query_properties.ungrouped(),
                    false,
                ),
                state.clone(),
                vec![],
                alias.clone(),
            )
        } else {
            let (multi_stage_member, is_ungrupped) = self
                .create_multi_stage_inode_member(member.clone(), resolved_multi_stage_dimensions)?;

            let mut dimensions_to_add = multi_stage_member.add_group_by_symbols().clone();

            if let Some(case) = member.case() {
                if let Some(switch_dim) = case.case_switch_dimension() {
                    dimensions_to_add.push(switch_dim);
                }
            }

            let directive_filter = multi_stage_filter_directive(&member);

            let needs_new_state = !dimensions_to_add.is_empty()
                || multi_stage_member.time_shift().is_some()
                || state.has_filters_for_member(&member_name)
                || directive_filter.is_some();

            let new_state = if needs_new_state {
                let mut new_state = match directive_filter.as_ref().map(|f| &f.mode) {
                    Some(MultiStageFilterMode::Fixed) => self.root_state().as_ref().clone(),
                    Some(MultiStageFilterMode::Relative) | None => state.as_ref().clone(),
                };

                if let Some(filter) = &directive_filter {
                    apply_filter_directive_to_state(filter, &mut new_state);
                }

                if !dimensions_to_add.is_empty() {
                    new_state.add_dimensions(dimensions_to_add.clone());
                }
                if let Some(time_shift) = multi_stage_member.time_shift() {
                    new_state.add_time_shifts(time_shift.clone())?;
                }
                if new_state.has_filters_for_member(&member_name) {
                    new_state.remove_filter_for_member(&member_name);
                }
                Rc::new(new_state)
            } else {
                state.clone()
            };

            let mut input = vec![];
            self.make_childs(
                member.clone(),
                new_state,
                &mut input,
                descriptions,
                resolved_multi_stage_dimensions,
                cte_state,
            )?;

            let alias = cte_state.next_cte_name();
            MultiStageQueryDescription::new(
                MultiStageMember::new(
                    MultiStageMemberType::Inode(multi_stage_member),
                    member,
                    is_ungrupped,
                    false,
                ),
                state.clone(),
                input,
                alias.clone(),
            )
        };

        descriptions.push(description.clone());
        Ok(description)
    }

    pub fn try_plan_rolling_window(
        &self,
        member: Rc<MemberSymbol>,
        state: Rc<QueryProperties>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
        resolved_multi_stage_dimensions: &mut HashSet<String>,
        cte_state: &mut CteState,
    ) -> Result<Option<Rc<MultiStageQueryDescription>>, CubeError> {
        if let Ok(measure) = member.as_measure() {
            if measure.is_cumulative() {
                let rolling_window = if let Some(rolling_window) = measure.rolling_window() {
                    rolling_window.clone()
                } else {
                    RollingWindow {
                        trailing: None,
                        leading: None,
                        offset: None,
                        rolling_type: None,
                        granularity: None,
                    }
                };

                if !measure.is_multi_stage() {
                    let childs = member_childs(&member, true)?;
                    let measures = childs
                        .iter()
                        .filter(|s| s.as_measure().is_ok())
                        .collect_vec();
                    if !measures.is_empty() {
                        return Err(CubeError::user(
                            format!("Measure {} references another measures ({}). In this case, {} must have multi_stage: true defined",
                            member.full_name(),
                            measures.into_iter().map(|m| m.full_name()).join(", "),
                            member.full_name(),
                                        ),
                        ));
                    }
                }

                let ungrouped = measure.is_rolling_window() && !measure.is_addictive();

                let mut time_dimensions = self
                    .query_properties
                    .time_dimensions()
                    .iter()
                    .map(|d| d.as_time_dimension())
                    .collect::<Result<Vec<_>, _>>()?;
                for dim in self.query_properties.dimensions() {
                    let dim = dim.clone().resolve_reference_chain();
                    if let Ok(time_dimension) = dim.as_time_dimension() {
                        time_dimensions.push(time_dimension);
                    }
                }

                let base_member = MemberSymbol::new_measure(measure.new_unrolling());

                if time_dimensions.is_empty() {
                    let base_state =
                        self.replace_date_range_for_rolling_window(&rolling_window, state.clone())?;
                    let rolling_base = if !measure.is_multi_stage() {
                        self.add_rolling_window_base(
                            base_member,
                            base_state,
                            false,
                            descriptions,
                            cte_state,
                        )?
                    } else {
                        self.make_queries_descriptions(
                            base_member,
                            base_state,
                            descriptions,
                            resolved_multi_stage_dimensions,
                            cte_state,
                        )?
                    };
                    return Ok(Some(rolling_base));
                }
                let uniq_time_dimensions = time_dimensions
                    .iter()
                    .unique_by(|a| (a.cube_name(), a.name(), a.date_range_vec()))
                    .collect_vec();
                if uniq_time_dimensions.len() != 1 {
                    return Err(CubeError::internal(
                        "Rolling window requires one time dimension and equal date ranges"
                            .to_string(),
                    ));
                }

                let time_dimension =
                    GranularityHelper::find_dimension_with_min_granularity(&time_dimensions)?;
                let time_dimension = MemberSymbol::new_time_dimension(time_dimension);

                let (base_rolling_state, base_time_dimension) = self.make_rolling_base_state(
                    time_dimension.clone(),
                    &rolling_window,
                    state.clone(),
                )?;

                let time_series =
                    self.add_time_series(time_dimension.clone(), state.clone(), descriptions)?;

                let rolling_base = if !measure.is_multi_stage() {
                    self.add_rolling_window_base(
                        base_member,
                        base_rolling_state,
                        ungrouped,
                        descriptions,
                        cte_state,
                    )?
                } else {
                    self.make_queries_descriptions(
                        base_member,
                        base_rolling_state,
                        descriptions,
                        resolved_multi_stage_dimensions,
                        cte_state,
                    )?
                };

                let input = vec![time_series, rolling_base];

                let alias = cte_state.next_cte_name();

                let rolling_window_descr = if measure.is_running_total() {
                    RollingWindowDescription::new_running_total(time_dimension, base_time_dimension)
                } else if let Some(granularity) =
                    self.get_to_date_rolling_granularity(&rolling_window)?
                {
                    RollingWindowDescription::new_to_date(
                        time_dimension,
                        base_time_dimension,
                        granularity,
                    )
                } else {
                    RollingWindowDescription::new_regular(
                        time_dimension,
                        base_time_dimension,
                        rolling_window.trailing.clone(),
                        rolling_window.leading.clone(),
                        rolling_window.offset.clone().unwrap_or("end".to_string()),
                    )
                };

                let inode_member = MultiStageInodeMember::new(
                    MultiStageInodeMemberType::RollingWindow(rolling_window_descr),
                    vec![],
                    vec![],
                    None,
                    None,
                );

                let description = MultiStageQueryDescription::new(
                    MultiStageMember::new(
                        MultiStageMemberType::Inode(inode_member),
                        member,
                        self.query_properties.ungrouped(),
                        false,
                    ),
                    state.clone(),
                    input,
                    alias.clone(),
                );
                descriptions.push(description.clone());
                Ok(Some(description))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn add_time_series_get_range_query(
        &self,
        time_dimension: Rc<MemberSymbol>,
        state: Rc<QueryProperties>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
    ) -> Result<Rc<MultiStageQueryDescription>, CubeError> {
        let description = if let Some(description) = descriptions
            .iter()
            .find(|d| d.alias() == "time_series_get_range")
        {
            description.clone()
        } else {
            let time_series_get_range_node = MultiStageQueryDescription::new(
                MultiStageMember::new(
                    MultiStageMemberType::Leaf(MultiStageLeafMemberType::TimeSeriesGetRange(
                        time_dimension.clone(),
                    )),
                    time_dimension.clone(),
                    true,
                    false,
                ),
                state.clone(),
                vec![],
                "time_series_get_range".to_string(),
            );
            descriptions.push(time_series_get_range_node.clone());
            time_series_get_range_node
        };
        Ok(description)
    }

    fn add_time_series(
        &self,
        time_dimension: Rc<MemberSymbol>,
        state: Rc<QueryProperties>,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
    ) -> Result<Rc<MultiStageQueryDescription>, CubeError> {
        let description = if let Some(description) =
            descriptions.iter().find(|d| d.alias() == "time_series")
        {
            description.clone()
        } else {
            let get_range_query_description = if time_dimension
                .as_time_dimension()?
                .date_range_vec()
                .is_some()
            {
                None
            } else {
                Some(self.add_time_series_get_range_query(
                    time_dimension.clone(),
                    state.clone(),
                    descriptions,
                )?)
            };
            let time_series_node = MultiStageQueryDescription::new(
                MultiStageMember::new(
                    MultiStageMemberType::Leaf(MultiStageLeafMemberType::TimeSeries(Rc::new(
                        TimeSeriesDescription {
                            time_dimension: time_dimension.clone(),
                            date_range_cte: get_range_query_description.map(|d| d.alias().clone()),
                        },
                    ))),
                    time_dimension.clone(),
                    true,
                    false,
                ),
                state.clone(),
                vec![],
                "time_series".to_string(),
            );
            descriptions.push(time_series_node.clone());
            time_series_node
        };
        Ok(description)
    }

    fn add_rolling_window_base(
        &self,
        member: Rc<MemberSymbol>,
        state: Rc<QueryProperties>,
        ungrouped: bool,
        descriptions: &mut Vec<Rc<MultiStageQueryDescription>>,
        cte_state: &mut CteState,
    ) -> Result<Rc<MultiStageQueryDescription>, CubeError> {
        let alias = cte_state.next_cte_name();
        let description = MultiStageQueryDescription::new(
            MultiStageMember::new(
                MultiStageMemberType::Leaf(MultiStageLeafMemberType::Measure),
                member,
                self.query_properties.ungrouped() || ungrouped,
                true,
            ),
            state,
            vec![],
            alias.clone(),
        );
        descriptions.push(description.clone());
        Ok(description)
    }

    fn get_to_date_rolling_granularity(
        &self,
        rolling_window: &RollingWindow,
    ) -> Result<Option<String>, CubeError> {
        let is_to_date = rolling_window
            .rolling_type
            .as_ref()
            .is_some_and(|tp| tp == "to_date");

        if is_to_date {
            if let Some(granularity) = &rolling_window.granularity {
                Ok(Some(granularity.clone()))
            } else {
                Err(CubeError::user(format!(
                    "Granularity required for to_date rolling window"
                )))
            }
        } else {
            Ok(None)
        }
    }

    /// Adjust date range filters for rolling window when there's no granularity.
    /// Without granularity there's no time_series CTE, so we replace InDateRange
    /// with BeforeOrOnDate/AfterOrOnDate that use parameters directly.
    fn replace_date_range_for_rolling_window(
        &self,
        rolling_window: &RollingWindow,
        state: Rc<QueryProperties>,
    ) -> Result<Rc<QueryProperties>, CubeError> {
        let mut new_state = state.as_ref().clone();
        for filter_item in state.time_dimensions_filters() {
            if let FilterItem::Item(filter) = filter_item {
                if matches!(filter.filter_operator(), FilterOperator::InDateRange) {
                    new_state.replace_date_range_for_rolling_window_without_granularity(
                        &filter.member_name(),
                        &rolling_window.trailing,
                        &rolling_window.leading,
                    )?;
                }
            }
        }
        Ok(Rc::new(new_state))
    }

    fn make_rolling_base_state(
        &self,
        time_dimension: Rc<MemberSymbol>,
        rolling_window: &RollingWindow,
        state: Rc<QueryProperties>,
    ) -> Result<(Rc<QueryProperties>, Rc<MemberSymbol>), CubeError> {
        let time_dimension_symbol = time_dimension.as_time_dimension()?;
        let time_dimension_base_name = time_dimension_symbol.base_symbol().full_name();
        let mut new_state = state.as_ref().clone();
        let trailing_granularity =
            GranularityHelper::granularity_from_interval(&rolling_window.trailing);
        let leading_granularity =
            GranularityHelper::granularity_from_interval(&rolling_window.leading);
        let window_granularity =
            GranularityHelper::min_granularity(&trailing_granularity, &leading_granularity)?;
        let result_granularity = GranularityHelper::min_granularity(
            &window_granularity,
            &time_dimension_symbol.resolved_granularity()?,
        )?;

        let new_time_dimension_symbol = time_dimension_symbol
            .change_granularity(self.query_tools.clone(), result_granularity.clone())?;
        let new_time_dimension = MemberSymbol::new_time_dimension(new_time_dimension_symbol);
        //We keep only one time_dimension in the leaf query because, even if time_dimension values have different granularity, in the leaf query we need to group by the lowest granularity.
        new_state.set_time_dimensions(vec![new_time_dimension.clone()]);

        let dimensions = new_state
            .dimensions()
            .clone()
            .into_iter()
            .filter(|d| {
                d.clone()
                    .resolve_reference_chain()
                    .as_time_dimension()
                    .is_err()
            })
            .collect_vec();
        new_state.set_dimensions(dimensions);

        if let Some(granularity) = self.get_to_date_rolling_granularity(rolling_window)? {
            new_state.replace_to_date_date_range_filter(&time_dimension_base_name, &granularity)?;
        } else {
            new_state.replace_regular_date_range_filter(
                &time_dimension_base_name,
                rolling_window.trailing.clone(),
                rolling_window.leading.clone(),
            )?;
        }

        Ok((Rc::new(new_state), new_time_dimension))
    }
}

fn multi_stage_filter_directive(member: &Rc<MemberSymbol>) -> Option<MultiStageFilter> {
    if let Ok(measure) = member.as_measure() {
        return measure.multi_stage().and_then(|m| m.filter.clone());
    }
    if let Ok(dimension) = member.as_dimension() {
        return dimension.multi_stage().and_then(|m| m.filter.clone());
    }
    None
}

//
// TODO: known interaction gaps when `mode: fixed` resets to `root_state`
// in chains. Both manifest only when a multi-stage member with `mode: fixed`
// is computed as a dependency of another node that already mutated state.
//
// 1. Rolling window. `try_plan_rolling_window` builds `base_rolling_state`
//    via `make_rolling_base_state` (extends date_range, swaps the time
//    dimension, prunes time-dim entries from `dimensions`). When a nested
//    multi-stage with `mode: fixed` is reached during recursion, it falls
//    back to `self.root_state`, dropping those rolling-window-specific
//    mutations — the leaf will read the original (narrow) date range while
//    the outer rolling frame expects the extended one.
//
// 2. Switch-case pruning. `apply_static_filter_to_symbol` runs at the top
//    of `make_queries_descriptions` against `state.dimensions_filters()` —
//    the *inherited* filters, before this function. If the inherited set
//    restricts the switch dimension, case branches are pruned at symbol
//    level; the subsequent `mode: fixed` reset cannot un-prune them.
//
// `add_dimension_evaluator` wraps segment references into a `MemberExpression`
// whose `full_name()` is prefixed with `expr:` (e.g. `expr:orders.completed`).
// `BaseSegment::full_name()` carries the bare path (`orders.completed`). To make
// `exclude`/`keep_only` match both forms, return the symbol's `full_name()`
// alongside its `expr:`-stripped variant.
fn filter_directive_match_names(symbol: &Rc<MemberSymbol>) -> Vec<String> {
    let full = symbol.full_name();
    if let Some(stripped) = full.strip_prefix("expr:") {
        vec![full.clone(), stripped.to_string()]
    } else {
        vec![full]
    }
}

fn apply_filter_directive_to_state(filter: &MultiStageFilter, state: &mut QueryProperties) {
    if let Some(exclude) = &filter.exclude {
        let names: Vec<String> = exclude
            .iter()
            .flat_map(|s| filter_directive_match_names(s))
            .collect();
        state.remove_filters_for_members(&names);
    }
    if let Some(keep_only) = &filter.keep_only {
        let names: Vec<String> = keep_only
            .iter()
            .flat_map(|s| filter_directive_match_names(s))
            .collect();
        state.keep_only_filters_for_members(&names);
    }
    if !filter.include_dimension.is_empty() {
        state.add_dimension_filters(filter.include_dimension.clone());
    }
    if !filter.include_time_dimension.is_empty() {
        state.add_time_dimension_filters(filter.include_time_dimension.clone());
    }
    if !filter.include_measure.is_empty() {
        state.add_measure_filters(filter.include_measure.clone());
    }
}
