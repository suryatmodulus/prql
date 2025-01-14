//! This module contains the parser, which is responsible for converting a tree
//! of pest pairs into a tree of AST Items. It has a small function to call into
//! pest to get the parse tree / concrete syntaxt tree, and then a large
//! function for turning that into PRQL AST.
use anyhow::{anyhow, bail, Context, Result};
use itertools::Itertools;
use pest::iterators::Pairs;
use pest::Parser;
use pest_derive::Parser;

use super::ast::*;
use super::utils::*;
use crate::error::Error;
use crate::error::Reason;
use crate::error::Span;
use crate::error::WithErrorInfo;
#[derive(Parser)]
#[grammar = "prql.pest"]
struct PrqlParser;

pub(crate) type PestError = pest::error::Error<Rule>;
pub(crate) type PestRule = Rule;

/// Build an AST from a PRQL query string.
pub fn parse(string: &str) -> Result<Query> {
    let ast = ast_of_string(string, Rule::query)?;

    ast.item.into_query().map_err(|_| unreachable!())
}

/// Parse a string into an AST. Unlike [parse], this can start from any rule.
fn ast_of_string(string: &str, rule: Rule) -> Result<Node> {
    let pairs = parse_tree_of_str(string, rule)?;

    ast_of_parse_tree(pairs)?.into_only()
}

/// Parse a string into a parse tree / concrete syntax tree, made up of pest Pairs.
fn parse_tree_of_str(source: &str, rule: Rule) -> Result<Pairs<Rule>> {
    Ok(PrqlParser::parse(rule, source)?)
}

/// Parses a parse tree of pest Pairs into an AST.
fn ast_of_parse_tree(pairs: Pairs<Rule>) -> Result<Vec<Node>> {
    pairs
        // Exclude end-of-input at the moment.
        .filter(|pair| pair.as_rule() != Rule::EOI)
        .map(|pair| {
            let span = pair.as_span();

            let item = match pair.as_rule() {
                Rule::query => Item::Query(Query {
                    nodes: ast_of_parse_tree(pair.into_inner())?,
                }),
                Rule::list => Item::List(
                    ast_of_parse_tree(pair.into_inner())?
                        .into_iter()
                        .map(ListItem)
                        .collect(),
                ),
                Rule::expr | Rule::expr_simple => ast_of_parse_tree(pair.into_inner())?.into_expr(),
                Rule::named_expr | Rule::named_expr_simple | Rule::named_term_simple => {
                    let items = ast_of_parse_tree(pair.into_inner())?;
                    // this borrow could be removed, but it becomes much less readable without match
                    match &items[..] {
                        [Node {
                            item: Item::Ident(name),
                            ..
                        }, node] => Item::NamedExpr(NamedExpr {
                            name: name.clone(),
                            expr: Box::new(node.clone()),
                        }),
                        [node] => node.item.clone(),
                        _ => unreachable!(),
                    }
                }
                Rule::transformation => {
                    let parsed = ast_of_parse_tree(pair.into_inner())?;
                    Item::Transform(ast_of_transformation(parsed)?)
                }
                Rule::func_def => {
                    let parsed = ast_of_parse_tree(pair.into_inner())?;

                    let (name, parsed) = parsed
                        .split_first()
                        .ok_or_else(|| anyhow!("Expected at least one item"))?;

                    let name = name.item.clone().into_ident()?;

                    let (params, body) = if let Some((
                        Node {
                            item: Item::Expr(params),
                            ..
                        },
                        body,
                    )) = parsed.split_first()
                    {
                        (params, body)
                    } else {
                        unreachable!("expected function params and body, got {parsed:?}")
                    };

                    let positional_params = params
                        .iter()
                        .filter(|x| matches!(x.item, Item::Ident(_)))
                        .cloned()
                        .collect();
                    let named_params = params
                        .iter()
                        .filter(|x| matches!(x.item, Item::NamedExpr(_)))
                        .cloned()
                        .collect();

                    Item::FuncDef(FuncDef {
                        name,
                        positional_params,
                        named_params,
                        body: Box::from(body.into_only().cloned()?),
                    })
                }
                Rule::func_def_params => Item::Expr(ast_of_parse_tree(pair.into_inner())?),
                Rule::func_call | Rule::func_curry => {
                    let items = ast_of_parse_tree(pair.into_inner())?;

                    let (name, params) = items
                        .split_first()
                        .ok_or_else(|| anyhow!("Expected at least one item"))?;

                    let name = name.item.clone().into_ident()?;

                    let (named, args): (Vec<_>, Vec<_>) = params
                        .iter()
                        .partition(|x| matches!(x.item, Item::NamedExpr(_)));

                    let args = args.into_iter().cloned().collect();

                    let named_args = named
                        .into_iter()
                        .cloned()
                        .map(|x| x.item.into_named_expr())
                        .try_collect()?;

                    Item::FuncCall(FuncCall {
                        name,
                        args,
                        named_args,
                    })
                }
                Rule::table => {
                    let parsed = ast_of_parse_tree(pair.into_inner())?;
                    let [name, pipeline]: [Node; 2] = parsed
                        .try_into()
                        .map_err(|e| anyhow!("Expected two items; {e:?}"))?;
                    Item::Table(Table {
                        name: name.item.into_ident()?,
                        pipeline: pipeline.item.into_pipeline()?,
                    })
                }
                Rule::ident => Item::Ident(pair.as_str().to_string()),
                // Pull out the string itself, which doesn't have the quotes
                Rule::string_literal => {
                    ast_of_parse_tree(pair.clone().into_inner())?
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("Failed reading string {pair:?}"))?
                        .item
                }
                Rule::string => Item::String(pair.as_str().to_string()),
                Rule::s_string => Item::SString(
                    pair.into_inner()
                        .map(|x| {
                            Ok(match x.as_rule() {
                                Rule::s_string_string => {
                                    SStringItem::String(x.as_str().to_string())
                                }
                                _ => SStringItem::Expr(
                                    ast_of_parse_tree(x.into_inner())?.into_expr().into(),
                                ),
                            })
                        })
                        .collect::<Result<Vec<SStringItem>>>()?,
                ),
                Rule::pipeline => Item::Pipeline({
                    ast_of_parse_tree(pair.into_inner())?
                        .into_iter()
                        .map(|x| match x {
                            Node {
                                item: Item::Transform(transformation),
                                ..
                            } => transformation,
                            _ => unreachable!("{x:?}"),
                        })
                        .collect()
                }),
                Rule::inline_pipeline => {
                    let parsed = ast_of_parse_tree(pair.into_inner())?;

                    let (value, func_curries) =
                        (parsed.split_first()).context("empty inline pipeline?")?;

                    let functions = func_curries.to_vec();

                    Item::InlinePipeline(InlinePipeline {
                        value: Box::from(value.clone()),
                        functions,
                    })
                }
                Rule::operator | Rule::number => Item::Raw(pair.as_str().to_owned()),
                _ => unreachable!(),
            };

            let mut node = Node::from(item);
            node.span = Span {
                start: span.start(),
                end: span.end(),
            };
            Ok(node)
        })
        .collect()
}

fn ast_of_transformation(items: Vec<Node>) -> Result<Transform> {
    let (name, args) = items
        .split_first()
        .ok_or_else(|| anyhow!("Expected at least one item"))?;

    let args: Vec<Node> = args.to_vec();

    let name = name.item.clone().into_ident()?;
    Ok(match name.as_str() {
        "from" => {
            let (name, expr) = args
                .into_only_node("from", "argument")
                .with_help("you can pass it as `from table_name` or `from alias: table_name`")?
                .into_name_and_expr();

            let table_ref = TableRef {
                name: expr.unwrap(Item::into_ident, "ident").with_help(
                    "`from` does not support inline expressions. You can only pass a table name.",
                )?,
                alias: name,
            };

            Transform::From(table_ref)
        }
        "select" => Transform::Select(args.into_only_node("select", "argument")?.coerce_to_items()),
        "filter" => {
            let items = args
                .into_only_node("filter", "argument")?
                .discard_name()?
                .coerce_to_items();
            Transform::Filter(Filter(items))
        }
        "derive" => {
            let assigns = (args)
                .into_only_node("derive", "argument")?
                .coerce_to_items();
            Transform::Derive(assigns)
        }
        "aggregate" => {
            // TODO: redo, generalizing with checks on custom functions.
            // Ideally we'd be able to add to the error message with context
            // without falling afowl of the borrow rules.
            // Err(anyhow!(
            //     "Expected exactly one unnamed argument for aggregate, got {:?}",
            //     args
            // ))
            // })?;
            let (positional, [by]) = unpack_arguments(args, ["by"]);
            let by = by.map(|x| x.coerce_to_items()).unwrap_or_default();

            let select = positional
                .into_only()
                .map(|x| x.coerce_to_items())
                .unwrap_or_default();

            Transform::Aggregate { by, select }
        }
        "sort" => {
            let by = args
                .into_only_node("sort", "argument")?
                .discard_name()?
                .coerce_to_items();
            Transform::Sort(by)
        }
        "take" => {
            // TODO: coerce to number
            let expr = args.into_only_node("take", "argument")?.discard_name()?;
            Transform::Take(expr.item.into_raw()?.parse()?)
        }
        "join" => {
            let (positional, [side]) = unpack_arguments(args, ["side"]);
            let side = if let Some(side) = side {
                let span = side.span;
                let ident = side.unwrap(Item::into_ident, "ident")?;
                match ident.as_str() {
                    "inner" => JoinSide::Inner,
                    "left" => JoinSide::Left,
                    "right" => JoinSide::Right,
                    "full" => JoinSide::Full,

                    found => bail!(Error::new(Reason::Expected {
                        who: Some("side".to_string()),
                        expected: "inner, left, right or full".to_string(),
                        found: found.to_string()
                    })
                    .with_span(span)),
                }
            } else {
                JoinSide::Inner
            };

            let with =
                (positional.get(0).cloned()).context("join requires a table name to join with")?;
            let (with_alias, with) = with.into_name_and_expr();

            let with = TableRef {
                name: with.unwrap(Item::into_ident, "ident").with_help(
                    "`join` does not support inline expressions. You can only pass a table name.",
                )?,
                alias: with_alias,
            };

            let on = if let Some(on) = positional.get(1) {
                on.clone().discard_name()?.coerce_to_items()
            } else {
                vec![]
            };

            Transform::Join { side, with, on }
        }
        _ => bail!("Expected a known transformation; got {name}"),
    })
}

/// Compares expected and passed function parameters
/// Returns positional and named parameters.
fn unpack_arguments<const COUNT: usize>(
    passed: Vec<Node>,
    expected: [&str; COUNT],
) -> (Vec<Node>, [Option<Node>; COUNT]) {
    let mut positional = Vec::new();

    const NONE: Option<Node> = None;
    let mut named = [NONE; COUNT];

    for p in passed {
        // Quite inefficient when number of arguments > 10. We could instead use merge join.
        if let Some(n) = p.item.as_named_expr() {
            if let Some((pos, _)) = expected.iter().find_position(|x| x == &&n.name) {
                named[pos] = Some(*p.item.into_named_expr().unwrap().expr);
                continue;
            }
        }

        positional.push(p);
    }
    (positional, named)
}

#[cfg(test)]
mod test {

    use super::*;
    use insta::{assert_debug_snapshot, assert_yaml_snapshot};

    #[test]
    fn test_parse_take() -> Result<()> {
        assert!(parse_tree_of_str("take 10", Rule::transformation).is_ok());
        assert!(ast_of_string("take", Rule::transformation).is_err());
        Ok(())
    }

    #[test]
    fn test_parse_string() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(r#"" U S A ""#, Rule::string_literal)?, @r###"
        [
            Pair {
                rule: string_literal,
                span: Span {
                    str: "\" U S A \"",
                    start: 0,
                    end: 9,
                },
                inner: [
                    Pair {
                        rule: string,
                        span: Span {
                            str: " U S A ",
                            start: 1,
                            end: 8,
                        },
                        inner: [],
                    },
                ],
            },
        ]
        "###);
        let double_quoted_ast = ast_of_string(r#"" U S A ""#, Rule::string_literal)?;
        assert_yaml_snapshot!(double_quoted_ast, @r###"
        ---
        String: " U S A "
        "###);

        let single_quoted_ast = ast_of_string(r#"' U S A '"#, Rule::string_literal)?;
        assert_eq!(single_quoted_ast, double_quoted_ast);

        // Single quotes within double quotes should produce a string containing
        // the single quotes (and vice versa).
        assert_yaml_snapshot!( ast_of_string(r#""' U S A '""#, Rule::string_literal)? , @r###"
        ---
        String: "' U S A '"
        "###);
        assert_yaml_snapshot!( ast_of_string(r#"'" U S A "'"#, Rule::string_literal)? , @r###"
        ---
        String: "\" U S A \""
        "###);

        assert!(ast_of_string(r#"" U S A"#, Rule::string_literal).is_err());
        assert!(ast_of_string(r#"" U S A '"#, Rule::string_literal).is_err());

        Ok(())
    }

    #[test]
    fn test_parse_s_string() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(r#"s"SUM({col})""#, Rule::s_string)?);
        assert_yaml_snapshot!(ast_of_string(r#"s"SUM({col})""#, Rule::s_string)?, @r###"
        ---
        SString:
          - String: SUM(
          - Expr:
              Ident: col
          - String: )
        "###);
        assert_yaml_snapshot!(ast_of_string(r#"s"SUM({2 + 2})""#, Rule::s_string)?, @r###"
        ---
        SString:
          - String: SUM(
          - Expr:
              Expr:
                - Raw: "2"
                - Raw: +
                - Raw: "2"
          - String: )
        "###);
        Ok(())
    }

    #[test]
    fn test_parse_list() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(r#"[1 + 1, 2]"#, Rule::list)?);
        assert_yaml_snapshot!(ast_of_string(r#"[1 + 1, 2]"#, Rule::list)?, @r###"
        ---
        List:
          - Expr:
              - Raw: "1"
              - Raw: +
              - Raw: "1"
          - Raw: "2"
        "###);
        assert_yaml_snapshot!(ast_of_string(r#"[1 + f 1, 2]"#, Rule::list)?, @r###"
        ---
        List:
          - Expr:
              - Raw: "1"
              - Raw: +
              - FuncCall:
                  name: f
                  args:
                    - Raw: "1"
                  named_args: []
          - Raw: "2"
        "###);
        let ab = ast_of_string(r#"[a b]"#, Rule::list)?;
        let a_comma_b = ast_of_string(r#"[a, b]"#, Rule::list)?;
        assert_yaml_snapshot!(ab, @r###"
        ---
        List:
          - FuncCall:
              name: a
              args:
                - Ident: b
              named_args: []
        "###);
        assert_yaml_snapshot!(a_comma_b, @r###"
        ---
        List:
          - Ident: a
          - Ident: b
        "###);
        assert_ne!(ab, a_comma_b);
        Ok(())
    }

    #[test]
    fn test_parse_number() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(r#"23"#, Rule::number)?, @r###"
        [
            Pair {
                rule: number,
                span: Span {
                    str: "23",
                    start: 0,
                    end: 2,
                },
                inner: [],
            },
        ]
        "###);
        assert_debug_snapshot!(parse_tree_of_str(r#"2 + 2"#, Rule::expr)?);
        Ok(())
    }

    #[test]
    fn test_parse_filter() -> Result<()> {
        assert_yaml_snapshot!(
            ast_of_string(r#"filter country = "USA""#, Rule::transformation)?
        , @r###"
        ---
        Transform:
          Filter:
            - Expr:
                - Ident: country
                - Raw: "="
                - String: USA
        "###);
        // TODO: Shoud the next two be different, based on whether there are
        // parentheses? I think possibly not.
        assert_yaml_snapshot!(
            ast_of_string(r#"filter (upper country) = "USA""#, Rule::transformation)?
        , @r###"
        ---
        Transform:
          Filter:
            - Expr:
                - FuncCall:
                    name: upper
                    args:
                      - Ident: country
                    named_args: []
                - Raw: "="
                - String: USA
        "###);

        let res = ast_of_string(r#"filter upper country = "USA""#, Rule::transformation);
        assert!(res.is_err());

        Ok(())
    }

    #[test]
    fn test_parse_aggregate() -> Result<()> {
        let aggregate = ast_of_string(
            "aggregate by:[title] [sum salary, count]",
            Rule::transformation,
        )?;
        assert_yaml_snapshot!(
            aggregate, @r###"
        ---
        Transform:
          Aggregate:
            by:
              - Ident: title
            select:
              - FuncCall:
                  name: sum
                  args:
                    - Ident: salary
                  named_args: []
              - Ident: count
        "###);
        let aggregate = ast_of_string("aggregate by:[title] [sum salary]", Rule::transformation)?;
        assert_yaml_snapshot!(
            aggregate, @r###"
        ---
        Transform:
          Aggregate:
            by:
              - Ident: title
            select:
              - FuncCall:
                  name: sum
                  args:
                    - Ident: salary
                  named_args: []
        "###);

        let node = ast_of_string("aggregate by:[title] [sum salary]", Rule::transformation)?;
        let aggregate = (node.item.as_transform()).ok_or_else(|| anyhow!("Expected Raw"))?;
        assert!(if let Transform::Aggregate { by, .. } = aggregate {
            by.len() == 1
                && by[0]
                    .item
                    .as_ident()
                    .ok_or_else(|| anyhow!("Expected Ident"))?
                    == "title"
        } else {
            false
        });

        assert_yaml_snapshot!(
            ast_of_string("aggregate by:[title] [sum salary]", Rule::transformation)?, @r###"
        ---
        Transform:
          Aggregate:
            by:
              - Ident: title
            select:
              - FuncCall:
                  name: sum
                  args:
                    - Ident: salary
                  named_args: []
        "###);
        Ok(())
    }

    #[test]
    fn test_parse_select() -> Result<()> {
        assert_yaml_snapshot!(
            ast_of_string(r#"select x"#, Rule::transformation)?
        , @r###"
        ---
        Transform:
          Select:
            - Ident: x
        "###);

        assert_yaml_snapshot!(
            ast_of_string(r#"select [x, y]"#, Rule::transformation)?
        , @r###"
        ---
        Transform:
          Select:
            - Ident: x
            - Ident: y
        "###);

        Ok(())
    }

    #[test]
    fn test_parse_expr() -> Result<()> {
        assert_yaml_snapshot!(
            ast_of_string(r#"country = "USA""#, Rule::expr)?
        , @r###"
        ---
        Expr:
          - Ident: country
          - Raw: "="
          - String: USA
        "###);
        assert_yaml_snapshot!(ast_of_string(
                r#"[
  gross_salary: salary + payroll_tax,
  gross_cost  : gross_salary + benefits_cost,
]"#,
        Rule::list)?, @r###"
        ---
        List:
          - NamedExpr:
              name: gross_salary
              expr:
                Expr:
                  - Ident: salary
                  - Raw: +
                  - Ident: payroll_tax
          - NamedExpr:
              name: gross_cost
              expr:
                Expr:
                  - Ident: gross_salary
                  - Raw: +
                  - Ident: benefits_cost
        "###);
        assert_yaml_snapshot!(
            ast_of_string(
                "gross_salary: (salary + payroll_tax) * (1 + tax_rate)",
                Rule::named_expr,
            )?,
            @r###"
        ---
        NamedExpr:
          name: gross_salary
          expr:
            Expr:
              - Expr:
                  - Ident: salary
                  - Raw: +
                  - Ident: payroll_tax
              - Raw: "*"
              - Expr:
                  - Raw: "1"
                  - Raw: +
                  - Ident: tax_rate
        "###);
        Ok(())
    }

    #[test]
    fn test_parse_query() -> Result<()> {
        assert_yaml_snapshot!(ast_of_string(
            r#"
from employees
filter country = "USA"                        # Each line transforms the previous result.
derive [                                      # This adds columns / variables.
  gross_salary: salary + payroll_tax,
  gross_cost:   gross_salary + benefits_cost # Variables can use other variables.
]
filter gross_cost > 0
aggregate by:[title, country] [               # `by` are the columns to group by.
                   average salary,            # These are aggregation calcs run on each group.
                   sum salary,
                   average gross_salary,
                   sum gross_salary,
                   average gross_cost,
  sum_gross_cost: sum gross_cost,
  ct            : count,
]
sort sum_gross_cost
filter ct > 200
take 20
    "#
            .trim(),
            Rule::query,
        )?);
        Ok(())
    }

    #[test]
    fn test_parse_function() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(
            "func plus_one x = x + 1",
            Rule::func_def
        )?);
        assert_yaml_snapshot!(ast_of_string(
            "func identity x = x", Rule::func_def
        )?
        , @r###"
        ---
        FuncDef:
          name: identity
          positional_params:
            - Ident: x
          named_params: []
          body:
            Ident: x
        "###);
        assert_yaml_snapshot!(ast_of_string(
            "func plus_one x = (x + 1)", Rule::func_def
        )?
        , @r###"
        ---
        FuncDef:
          name: plus_one
          positional_params:
            - Ident: x
          named_params: []
          body:
            Expr:
              - Ident: x
              - Raw: +
              - Raw: "1"
        "###);
        assert_yaml_snapshot!(ast_of_string(
            "func plus_one x = x + 1", Rule::func_def
        )?
        , @r###"
        ---
        FuncDef:
          name: plus_one
          positional_params:
            - Ident: x
          named_params: []
          body:
            Expr:
              - Ident: x
              - Raw: +
              - Raw: "1"
        "###);
        // An example to show that we can't delayer the tree, despite there
        // being lots of layers.
        assert_yaml_snapshot!(ast_of_string(
            "func foo x = (foo bar + 1) (plax) - baz", Rule::func_def
        )?
        , @r###"
        ---
        FuncDef:
          name: foo
          positional_params:
            - Ident: x
          named_params: []
          body:
            Expr:
              - FuncCall:
                  name: foo
                  args:
                    - Ident: bar
                  named_args: []
              - Raw: +
              - Raw: "1"
        "###);

        assert_yaml_snapshot!(ast_of_string("func return_constant = 42", Rule::func_def)?, @r###"
        ---
        FuncDef:
          name: return_constant
          positional_params: []
          named_params: []
          body:
            Raw: "42"
        "###);
        assert_yaml_snapshot!(ast_of_string(r#"func count X = s"SUM({X})""#, Rule::func_def)?, @r###"
        ---
        FuncDef:
          name: count
          positional_params:
            - Ident: X
          named_params: []
          body:
            SString:
              - String: SUM(
              - Expr:
                  Ident: X
              - String: )
        "###);

        /* TODO: Does not yet parse because `window` not yet implemented.
            assert_debug_snapshot!(ast_of_parse_tree(
                parse_tree_of_str(
                    r#"
        func lag_day x = (
          window x
          by sec_id
          sort date
          lag 1
        )
                    "#,
                    Rule::func_def
                )
                .unwrap()
            ));
            */

        assert_yaml_snapshot!(ast_of_string(r#"func add x to:a = x + to"#, Rule::func_def)?, @r###"
        ---
        FuncDef:
          name: add
          positional_params:
            - Ident: x
          named_params:
            - NamedExpr:
                name: to
                expr:
                  Ident: a
          body:
            Expr:
              - Ident: x
              - Raw: +
              - Ident: to
        "###);

        Ok(())
    }

    #[test]
    fn test_parse_func_call() -> Result<()> {
        // Function without argument
        let ast = ast_of_string(r#"count"#, Rule::expr)?;
        let ident = ast.item.into_ident()?;
        assert_yaml_snapshot!(
            ident, @r###"
        ---
        count
        "###);

        // A non-friendly option for #154
        let ast = ast_of_string(r#"count s'*'"#, Rule::expr)?;
        let func_call: FuncCall = ast.item.into_func_call()?;
        assert_yaml_snapshot!(
            func_call, @r###"
        ---
        name: count
        args:
          - SString:
              - String: "*"
        named_args: []
        "###);

        assert_yaml_snapshot!(parse(r#"from mytable | select [a and b + c or d e and f]"#)?, @r###"
        ---
        nodes:
          - Pipeline:
              - From:
                  name: mytable
                  alias: ~
              - Select:
                  - Expr:
                      - Ident: a
                      - Raw: and
                      - Ident: b
                      - Raw: +
                      - Ident: c
                      - Raw: or
                      - FuncCall:
                          name: d
                          args:
                            - Ident: e
                          named_args: []
                      - Raw: and
                      - Ident: f
        "###);

        Ok(())
    }

    #[test]
    fn test_parse_table() -> Result<()> {
        assert_yaml_snapshot!(ast_of_string(
            "table newest_employees = ( from employees )",
            Rule::table
        )?, @r###"
        ---
        Table:
          name: newest_employees
          pipeline:
            - From:
                name: employees
                alias: ~
        "###);

        assert_yaml_snapshot!(ast_of_string(
            r#"
        table newest_employees = (
          from employees
          aggregate by:country [
            average_country_salary: average salary
          ]
          sort tenure
          take 50
        )"#.trim(), Rule::table)?,
         @r###"
        ---
        Table:
          name: newest_employees
          pipeline:
            - From:
                name: employees
                alias: ~
            - Aggregate:
                by:
                  - Ident: country
                select:
                  - NamedExpr:
                      name: average_country_salary
                      expr:
                        FuncCall:
                          name: average
                          args:
                            - Ident: salary
                          named_args: []
            - Sort:
                - Ident: tenure
            - Take: 50
        "###);
        Ok(())
    }

    #[test]
    fn test_parse_into_parse_tree() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(r#"country = "USA""#, Rule::expr)?);
        assert_debug_snapshot!(parse_tree_of_str("select [a, b, c]", Rule::transformation)?);
        assert_debug_snapshot!(parse_tree_of_str(
            "aggregate by:[title, country] [sum salary]",
            Rule::transformation
        )?);
        assert_debug_snapshot!(parse_tree_of_str(
            r#"    filter country = "USA""#,
            Rule::transformation
        )?);
        assert_debug_snapshot!(parse_tree_of_str("[a, b, c,]", Rule::list)?);
        assert_debug_snapshot!(parse_tree_of_str(
            r#"[
  gross_salary: salary + payroll_tax,
  gross_cost  : gross_salary + benefits_cost
]"#,
            Rule::list
        )?);
        // Currently not putting comments in our parse tree, so this is blank.
        assert_debug_snapshot!(parse_tree_of_str(
            r#"# this is a comment
        select a"#,
            Rule::COMMENT
        )?);
        assert_debug_snapshot!(parse_tree_of_str(
            "join country [id=employee_id]",
            Rule::transformation
        )?);
        assert_debug_snapshot!(parse_tree_of_str(
            "join side:left country [id=employee_id]",
            Rule::transformation
        )?);
        assert_debug_snapshot!(parse_tree_of_str("1  + 2", Rule::expr)?);
        Ok(())
    }

    #[test]
    fn test_inline_pipeline() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(
            "(salary | percentile 50)",
            Rule::inline_pipeline
        )?);
        assert_yaml_snapshot!(ast_of_string("(salary | percentile 50)", Rule::inline_pipeline)?, @r###"
        ---
        InlinePipeline:
          value:
            Ident: salary
          functions:
            - FuncCall:
                name: percentile
                args:
                  - Raw: "50"
                named_args: []
        "###);
        assert_yaml_snapshot!(ast_of_string("func median x = (x | percentile 50)", Rule::query)?, @r###"
        ---
        Query:
          nodes:
            - FuncDef:
                name: median
                positional_params:
                  - Ident: x
                named_params: []
                body:
                  InlinePipeline:
                    value:
                      Ident: x
                    functions:
                      - FuncCall:
                          name: percentile
                          args:
                            - Raw: "50"
                          named_args: []
        "###);

        Ok(())
    }

    #[test]
    fn test_parse_pipeline_parse_tree() -> Result<()> {
        assert_debug_snapshot!(parse_tree_of_str(
            r#"
    from employees
    select [a, b]
    "#
            .trim(),
            Rule::pipeline
        )?);
        assert_debug_snapshot!(parse_tree_of_str(
            r#"
    from employees
    filter country = "USA"
    "#
            .trim(),
            Rule::pipeline
        )?);
        assert_debug_snapshot!(parse_tree_of_str(
            r#"
from employees
filter country = "USA"                           # Each line transforms the previous result.
derive [                                         # This adds columns / variables.
  gross_salary: salary + payroll_tax,
  gross_cost  : gross_salary + benefits_cost    # Variables can use other variables.
]
filter gross_cost > 0
aggregate by:[title, country] [                  # `by` are the columns to group by.
    average salary,                              # These are aggregation calcs run on each group.
    sum     salary,
    average gross_salary,
    sum     gross_salary,
    average gross_cost,
    sum_gross_cost: sum gross_cost,
    count: count,
]
sort sum_gross_cost
filter count > 200
take 20
    "#
            .trim(),
            Rule::pipeline
        )?);
        Ok(())
    }

    #[test]
    fn test_parse_sql_parameters() -> Result<()> {
        assert_yaml_snapshot!(parse(r#"
        from mytable
        filter [
            first_name = $1,
            last_name = $2.name
        ]
        "#)?, @r###"
        ---
        nodes:
          - Pipeline:
              - From:
                  name: mytable
                  alias: ~
              - Filter:
                  - Expr:
                      - Ident: first_name
                      - Raw: "="
                      - Ident: $1
                  - Expr:
                      - Ident: last_name
                      - Raw: "="
                      - Ident: $2.name
        "###);
        Ok(())
    }

    #[test]
    fn test_tab_characters() -> Result<()> {
        // #284

        let prql = "from c_invoice
join doc:c_doctype [c_invoice_id]
select [
\tinvoice_no,
\tdocstatus
]";
        let result = parse(prql);
        assert!(result.is_ok());

        Ok(())
    }
}
