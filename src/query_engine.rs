use std::iter::Iterator;
use std::rc::Rc;
use std::collections::HashMap;
use std::collections::HashSet;
use time::precise_time_ns;
use std::ops::Add;

use value::ValueType;
use expression::*;
use aggregator::*;
use limit::*;
use util::fmt_table;
use columns::Column;
use columns::ColIter;
use columns::Batch;


#[derive(Debug)]
pub struct Query {
    pub select: Vec<Expr>,
    pub filter: Expr,
    pub limit: Option<LimitClause>,
    pub aggregate: Vec<(Aggregator, Expr)>,
}

pub struct QueryResult {
    pub colnames: Vec<Rc<String>>,
    pub rows: Vec<Vec<ValueType>>,
    pub stats: QueryStats,
}

pub struct QueryStats {
    pub runtime_ns: u64,
    pub rows_scanned: u64,
}

impl Add for QueryStats {
    type Output = QueryStats;

    fn add(self, other: QueryStats) -> QueryStats {
        QueryStats{
            runtime_ns: self.runtime_ns + other.runtime_ns,
            rows_scanned: self.rows_scanned + other.rows_scanned,
        }
    }
}


impl Query {
    pub fn run(&self, source: &Batch) -> QueryResult {
        let referenced_cols = self.find_referenced_cols();
        let efficient_source: Vec<&Box<Column>> = source.cols.iter().filter(|col| referenced_cols.contains(&col.get_name().to_string())).collect();
        let mut coliter = efficient_source.iter().map(|col| col.iter()).collect();

        let column_indices = create_colname_map(&efficient_source);
        let compiled_selects = self.select.iter().map(|expr| expr.compile(&column_indices)).collect();
        let compiled_filter = self.filter.compile(&column_indices);
        let compiled_aggregate = self.aggregate.iter().map(|&(agg, ref expr)| (agg, expr.compile(&column_indices))).collect();

        let start_time_ns = precise_time_ns();
        let (result_rows, rows_touched) = if self.aggregate.len() == 0 {
            run_select_query(&compiled_selects, &compiled_filter, &mut coliter)
        } else {
            run_aggregation_query(&compiled_selects, &compiled_filter, &compiled_aggregate, &mut coliter)
        };


        QueryResult {
            colnames: self.result_column_names(),
            rows: result_rows,
            stats: QueryStats {
                runtime_ns: precise_time_ns() - start_time_ns,
                rows_scanned: rows_touched,
            },
        }
    }

    pub fn run_batches(&self, batches: &Vec<Batch>) -> QueryResult {
        let mut combined_rows = Vec::new();
        let mut combined_stats = QueryStats { runtime_ns: 0, rows_scanned: 0 };
        for batch in batches {
            let QueryResult { rows, stats, .. } = self.run(batch);
            combined_rows.extend(rows); // TODO: This isn't the right way to combine results!!!
            combined_stats = combined_stats + stats;
        }
        QueryResult {
            colnames: self.result_column_names(),
            rows: combined_rows,
            stats: combined_stats,
        }
    }

    fn find_referenced_cols(&self) -> HashSet<Rc<String>> {
        let mut colnames = HashSet::new();
        for expr in self.select.iter() {
            expr.add_colnames(&mut colnames);
        }
        self.filter.add_colnames(&mut colnames);
        for &(_, ref expr) in self.aggregate.iter() {
            expr.add_colnames(&mut colnames);
        }
        colnames
    }

    fn result_column_names(&self) -> Vec<Rc<String>> {
        let mut anon_columns = -1;
        let select_cols = self.select
            .iter()
            .map(|expr| match expr {
                &Expr::ColName(ref name) => name.clone(),
                _ => {
                    anon_columns += 1;
                    Rc::new(format!("col_{}", anon_columns))
                },
            });
        let mut anon_aggregates = -1;
        let aggregate_cols = self.aggregate
            .iter()
            .map(|&(agg, _)| {
                anon_aggregates += 1;
                match agg {
                    Aggregator::Count => Rc::new(format!("count_{}", anon_aggregates)),
                    Aggregator::Sum => Rc::new(format!("sum_{}", anon_aggregates)),
                }
            });

        select_cols.chain(aggregate_cols).collect()
    }
}

fn create_colname_map(source: &Vec<&Box<Column>>) -> HashMap<String, usize> {
    let mut columns = HashMap::new();
    for (i, col) in source.iter().enumerate() {
        columns.insert(col.get_name().to_string(), i as usize);
    }
    columns
}

fn run_select_query(select: &Vec<Expr>, filter: &Expr, source: &mut Vec<ColIter>) -> (Vec<Vec<ValueType>>, u64) {
    let mut result = Vec::new();
    let mut record = Vec::with_capacity(source.len());
    let mut rows_touched = 0;
    let mut result_count = 0;
    if source.len() == 0 { return (result, rows_touched) }
    loop {
        record.clear();
        for i in 0..source.len() {
            match source[i].next() {
                Some(item) => record.push(item),
                None => return (result, rows_touched),
            }
        }
        if filter.eval(&record) == ValueType::Bool(true) {
            result.push(select.iter().map(|expr| expr.eval(&record)).collect());
            result_count += 1;
        }
        rows_touched += 1
        //TODO(limit)
        //if self.limit != None {
        //    if result_count > self.limit.limit {
        //        break;
        //    }
        //}
    }
}

fn run_aggregation_query(select: &Vec<Expr>, filter: &Expr, aggregation: &Vec<(Aggregator, Expr)>, source: &mut Vec<ColIter>) -> (Vec<Vec<ValueType>>, u64) {
    let mut groups: HashMap<Vec<ValueType>, Vec<ValueType>> = HashMap::new();
    let mut record = Vec::with_capacity(source.len());
    let mut rows_touched = 0;
    'outer: loop {
        record.clear();
        for i in 0..source.len() {
            match source[i].next() {
                Some(item) => record.push(item),
                None => break 'outer,
            }
        }
        if filter.eval(&record) == ValueType::Bool(true) {
            let group: Vec<ValueType> = select.iter().map(|expr| expr.eval(&record)).collect();
            let accumulator = groups.entry(group).or_insert(aggregation.iter().map(|x| x.0.zero()).collect());
            for (i, &(ref agg_func, ref expr)) in aggregation.iter().enumerate() {
                accumulator[i] = agg_func.reduce(&accumulator[i], &expr.eval(&record));
            }
        }
        if source.len() == 0 { break }
        rows_touched += 1;
    }

    let mut result: Vec<Vec<ValueType>> = Vec::new();
    for (mut group, aggregate) in groups {
        group.extend(aggregate);
        result.push(group);
    }
    (result, rows_touched)
}

pub fn print_query_result(results: &QueryResult) {
    let rt = results.stats.runtime_ns;
    let fmt_time = if rt < 10_000 {
        format!("{}ns", rt)
    } else if rt < 10_000_000 {
        format!("{}μs", rt / 1000)
    } else if rt < 10_000_000_000 {
        format!("{}ms", rt / 1_000_000)
    } else {
        format!("{}s", rt / 1_000_000_000)
    };

    println!("Scanned {} rows in {}!\n", results.stats.rows_scanned, fmt_time);
    println!("{}", format_results(&results.colnames, &results.rows));
}

fn format_results(colnames: &Vec<Rc<String>>, rows: &Vec<Vec<ValueType>>) -> String {
    let strcolnames: Vec<&str> = colnames.iter().map(|ref s| s.clone() as &str).collect();
    let formattedrows: Vec<Vec<String>> = rows.iter().map(
        |row| row.iter().map(
            |val| format!("{}", val)).collect()).collect();
    let strrows = formattedrows.iter().map(|row| row.iter().map(|val| val as &str).collect()).collect();

    fmt_table(&strcolnames, &strrows)
}

pub fn test(source: &Batch) {
    use self::Expr::*;
    use self::FuncType::*;
    use ValueType::*;

    let query1 = Query {
        select: vec![Expr::col("url")],
        filter: Expr::func(And,
                           Expr::func(LT, Expr::col("loadtime"), Const(Integer(1000))),
                           Expr::func(GT, Expr::col("timestamp"), Const(Timestamp(1000)))),
        aggregate: vec![],
        limit: None,
    };
    let query2 = Query {
        select: vec![Expr::col("timestamp"), Expr::col("loadtime")],
        filter: Expr::func(Equals, Expr::col("url"), Const(Str(Rc::new("/".to_string())))),
        aggregate: vec![],
        limit: None,
    };
    let count_query = Query {
        select: vec![Expr::col("url")],
        filter: Const(Bool(true)),
        aggregate: vec![(Aggregator::Count, Const(Integer(0)))],
        limit: None,
    };
    let sum_query = Query {
        select: vec![Expr::col("url")],
        filter: Const(Bool(true)),
        aggregate: vec![(Aggregator::Sum, Expr::col("loadtime"))],
        limit: None,
    };
    let missing_col_query = Query {
        select: vec![],
        filter: Const(Bool(true)),
        aggregate: vec![(Aggregator::Sum, Expr::col("doesntexist"))],
        limit: None,
    } ;

    //TODO(limit)
    //let limited_query = Query {
    //    select: vec![Expr::col("url")],
    //    filter: Expr::func(And,
    //                       Expr::func(LT, Expr::col("loadtime"), Const(Integer(1000))),
    //                       Expr::func(GT, Expr::col("timestamp"), Const(Timestamp(1000)))),
    //    aggregate: vec![],
    //    limit: LimitClause{ limit:3, offset:0 },
    //} ;

    let result1 = query1.run(source);
    let result2 = query2.run(source);
    let count_result = count_query.run(source);
    let sum_result = sum_query.run(source);
    let missing_col_result = missing_col_query.run(source);
    //let limited_result = limited_query.run(source);

    print_query_result(&result1);
    print_query_result(&result2);
    print_query_result(&count_result);
    print_query_result(&sum_result);
    print_query_result(&missing_col_result);
    //print_query_result(&limited_result);
}
