// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! InfluxQL planner

use std::{cell::OnceCell, sync::Arc};

use arrow::datatypes::SchemaRef as ArrowSchemaRef;
use datafusion::{
    error::DataFusionError, logical_expr::TableSource, sql::planner::ContextProvider,
};
use generic_error::BoxError;
use influxql_logical_planner::plan::{
    ceresdb_schema_to_influxdb, InfluxQLToLogicalPlan, SchemaProvider,
};
use influxql_parser::{
    common::{MeasurementName, QualifiedMeasurementName},
    select::{MeasurementSelection, SelectStatement},
    show_measurements::ShowMeasurementsStatement,
    statement::Statement as InfluxqlStatement,
};
use influxql_schema::Schema;
use logger::error;
use snafu::{ensure, ResultExt};
use table_engine::table::TableRef;

use crate::{
    influxql::error::*,
    logical_optimizer::optimize_plan,
    plan::{Plan, QueryPlan, QueryType, ShowPlan, ShowTablesPlan},
    provider::{ContextProviderAdapter, MetaProvider},
};

// Same with iox
pub const HORAEDB_MEASUREMENT_COLUMN_NAME: &str = "iox::measurement";

// Port from https://github.com/CeresDB/influxql/blob/36fc4d873e/iox_query_influxql/src/frontend/planner.rs#L28
struct InfluxQLSchemaProvider<'a, P: MetaProvider> {
    context_provider: ContextProviderAdapter<'a, P>,
    tables_cache: OnceCell<Vec<TableRef>>,
}

impl<'a, P: MetaProvider> SchemaProvider for InfluxQLSchemaProvider<'a, P> {
    fn get_table_provider(&self, name: &str) -> datafusion::error::Result<Arc<dyn TableSource>> {
        self.context_provider
            .get_table_provider(name.into())
            .map_err(|e| {
                DataFusionError::Plan(format!(
                    "measurement does not exist, measurement:{name}, source:{e}"
                ))
            })
    }

    fn get_function_meta(&self, name: &str) -> Option<Arc<datafusion::logical_expr::ScalarUDF>> {
        self.context_provider.get_function_meta(name)
    }

    fn get_aggregate_meta(
        &self,
        name: &str,
    ) -> Option<Arc<datafusion::logical_expr::AggregateUDF>> {
        self.context_provider.get_aggregate_meta(name)
    }

    fn table_names(&self) -> Vec<&'_ str> {
        let tables = match self
            .tables_cache
            .get_or_try_init(|| self.context_provider.all_tables())
        {
            Ok(tables) => tables,
            Err(e) => {
                // Restricted by the external interface of iox, we can just print error log here
                // and return empty `Vec`.
                error!("Influxql planner failed to get all tables, err:{e}");
                return Vec::default();
            }
        };

        tables.iter().map(|t| t.name()).collect()
    }

    fn table_schema(&self, name: &str) -> Option<Schema> {
        let table_source = match self.get_table_provider(name) {
            Ok(table) => table,
            Err(e) => {
                // Restricted by the external interface of iox, we can just print error log here
                // and return None.
                error!("Influxql planner failed to get table schema, name:{name}, err:{e}");
                return None;
            }
        };

        let horaedb_arrow_schema = table_source.schema();
        let influxql_schema = match convert_to_influxql_schema(table_source.schema()) {
            Ok(schema) => schema,
            Err(e) => {
                // Same as above here.
                error!("Influxql planner failed to convert schema to influxql schema, schema:{horaedb_arrow_schema}, err:{e}");
                return None;
            }
        };

        Some(influxql_schema)
    }

    fn table_exists(&self, name: &str) -> bool {
        match self.context_provider.table(name.into()) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                // Same as above here.
                error!("Influxql planner failed to find table, table_name:{name}, err:{e}");
                false
            }
        }
    }
}

/// Influxql logical planner
///
/// NOTICE: planner will be built for each influxql query.
pub(crate) struct Planner<'a, P: MetaProvider> {
    schema_provider: InfluxQLSchemaProvider<'a, P>,
}

fn convert_to_influxql_schema(horaedb_arrow_schema: ArrowSchemaRef) -> Result<Schema> {
    ceresdb_schema_to_influxdb(horaedb_arrow_schema)
        .box_err()
        .and_then(|s| Schema::try_from(s).box_err())
        .context(BuildPlanWithCause {
            msg: "build influxql schema",
        })
}

impl<'a, P: MetaProvider> Planner<'a, P> {
    pub fn new(context_provider: ContextProviderAdapter<'a, P>) -> Self {
        Self {
            schema_provider: InfluxQLSchemaProvider {
                context_provider,
                tables_cache: OnceCell::new(),
            },
        }
    }

    /// Build sql logical plan from [InfluxqlStatement].
    ///
    /// NOTICE: when building plan from influxql select statement,
    /// the [InfluxqlStatement] will be converted to [SqlStatement] first,
    /// and build plan then.
    pub fn statement_to_plan(self, stmt: InfluxqlStatement) -> Result<Plan> {
        match stmt {
            // TODO: show measurement is a temp workaround, it should be implemented in influxql
            // crates.
            InfluxqlStatement::ShowMeasurements(stmt) => self.show_measurements_to_plan(*stmt),
            _ => {
                let planner = InfluxQLToLogicalPlan::new(&self.schema_provider);
                let df_plan =
                    planner
                        .statement_to_plan(stmt)
                        .box_err()
                        .context(BuildPlanWithCause {
                            msg: "planner stmt to plan",
                        })?;
                let df_plan = optimize_plan(&df_plan)
                    .box_err()
                    .context(BuildPlanWithCause {
                        msg: "optimize plan",
                    })?;

                let tables = Arc::new(
                    self.schema_provider
                        .context_provider
                        .try_into_container()
                        .box_err()
                        .context(BuildPlanWithCause {
                            msg: "get tables from context_provider",
                        })?,
                );
                Ok(Plan::Query(QueryPlan {
                    df_plan,
                    tables,
                    table_name: None,
                }))
            }
        }
    }

    // TODO: support offset/limit/match in stmt
    fn show_measurements_to_plan(self, _stmt: ShowMeasurementsStatement) -> Result<Plan> {
        let plan = ShowTablesPlan {
            pattern: None,
            query_type: QueryType::InfluxQL,
        };
        Ok(Plan::Show(ShowPlan::ShowTablesPlan(plan)))
    }
}

pub fn check_select_statement(select_stmt: &SelectStatement) -> Result<()> {
    // Only support from single measurements now.
    ensure!(
        !select_stmt.from.is_empty(),
        BuildPlanNoCause {
            msg: format!("invalid influxql select statement with empty from, stmt:{select_stmt}"),
        }
    );
    ensure!(
        select_stmt.from.len() == 1,
        Unimplemented {
            msg: format!("select from multiple measurements, stmt:{select_stmt}"),
        }
    );

    let from = &select_stmt.from[0];
    match from {
        MeasurementSelection::Name(name) => {
            let QualifiedMeasurementName { name, .. } = name;

            match name {
                MeasurementName::Regex(_) => Unimplemented {
                    msg: format!("select from regex, stmt:{select_stmt}"),
                }
                .fail(),
                MeasurementName::Name(_) => Ok(()),
            }
        }
        MeasurementSelection::Subquery(_) => Unimplemented {
            msg: format!("select from subquery, stmt:{select_stmt}"),
        }
        .fail(),
    }
}

#[cfg(test)]
mod test {
    use influxql_parser::{select::SelectStatement, statement::Statement};

    use super::check_select_statement;

    #[test]
    fn test_check_select_from() {
        let from_measurement = parse_select("select * from a;");
        let from_multi_measurements = parse_select("select * from a,b;");
        let from_regex = parse_select(r#"select * from /d/"#);
        let from_subquery = parse_select("select * from (select a,b from c)");

        let res = check_select_statement(&from_measurement);
        assert!(res.is_ok());

        let res = check_select_statement(&from_multi_measurements);
        let err = res.err().unwrap();
        assert!(err
            .to_string()
            .contains("select from multiple measurements"));

        let res = check_select_statement(&from_regex);
        let err = res.err().unwrap();
        assert!(err.to_string().contains("select from regex"));

        let res = check_select_statement(&from_subquery);
        let err = res.err().unwrap();
        assert!(err.to_string().contains("select from subquery"));
    }

    fn parse_select(influxql: &str) -> SelectStatement {
        let stmt = influxql_parser::parse_statements(influxql).unwrap()[0].clone();
        if let Statement::Select(select_stmt) = stmt {
            *select_stmt
        } else {
            unreachable!()
        }
    }
}
