use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::model::{Catalog, Column, ScalarType, Value};
use crate::query::{BoolExpression, CmpOp, ScalarExpression};

pub const MAX_INDEX_PREDICATE_ATOMS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPredicate {
    pub atoms: Vec<IndexPredicateAtom>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IndexPredicateAtom {
    Equal {
        column: String,
        field_path: Vec<u64>,
        value_type: ScalarType,
        value: Value,
    },
    IsNone {
        column: String,
        field_path: Vec<u64>,
    },
    IsSome {
        column: String,
        field_path: Vec<u64>,
    },
}

impl IndexPredicate {
    pub(crate) fn bind(
        catalog: &Catalog,
        fields: &[Column],
        expression: &BoolExpression,
    ) -> Result<Self> {
        validate_shape(expression)?;
        let mut bound = expression.clone();
        crate::expression::bind(catalog, fields, &mut bound)?;
        let mut atoms = Vec::new();
        collect_atoms(catalog, fields, &bound, &mut atoms)?;
        if atoms.len() > MAX_INDEX_PREDICATE_ATOMS {
            return Err(Error::new(
                "E_INDEX_PREDICATE",
                format!(
                    "partial unique index predicates support at most {MAX_INDEX_PREDICATE_ATOMS} atoms"
                ),
            ));
        }
        normalize(atoms)
    }

    pub(crate) fn identity_key(&self) -> String {
        let identity = self
            .atoms
            .iter()
            .map(|atom| (atom.field_path().to_vec(), atom_payload_key(atom)))
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&identity)
            .expect("serializing a bound index predicate to memory cannot fail");
        format!("sha256:{:x}", Sha256::digest(encoded))
    }

    pub(crate) fn source_text(&self) -> String {
        self.atoms
            .iter()
            .map(IndexPredicateAtom::source_text)
            .collect::<Vec<_>>()
            .join(" && ")
    }
}

impl IndexPredicateAtom {
    pub(crate) fn column(&self) -> &str {
        match self {
            Self::Equal { column, .. }
            | Self::IsNone { column, .. }
            | Self::IsSome { column, .. } => column,
        }
    }

    pub(crate) fn field_path(&self) -> &[u64] {
        match self {
            Self::Equal { field_path, .. }
            | Self::IsNone { field_path, .. }
            | Self::IsSome { field_path, .. } => field_path,
        }
    }

    fn source_text(&self) -> String {
        match self {
            Self::Equal { column, value, .. } => {
                format!("{column} == {}", value.source_text())
            }
            Self::IsNone { column, .. } => format!("{column} == None"),
            Self::IsSome { column, .. } => format!("is_some {column}"),
        }
    }
}

fn validate_shape(expression: &BoolExpression) -> Result<()> {
    match expression {
        BoolExpression::And(left, right) => {
            validate_shape(left)?;
            validate_shape(right)
        }
        BoolExpression::Compare {
            left: ScalarExpression::Reference(_),
            op: CmpOp::Eq,
            right: ScalarExpression::Literal(_),
            ..
        }
        | BoolExpression::IsNone(ScalarExpression::Reference(_))
        | BoolExpression::IsSome(ScalarExpression::Reference(_)) => Ok(()),
        BoolExpression::Compare { op, .. } if *op != CmpOp::Eq => Err(Error::new(
            "E_INDEX_PREDICATE",
            "partial unique index predicates only support '==' comparisons",
        )),
        BoolExpression::Compare { .. } => Err(Error::new(
            "E_INDEX_PREDICATE",
            "partial unique index equality must compare a field path with a literal",
        )),
        BoolExpression::IsNone(_) | BoolExpression::IsSome(_) => Err(Error::new(
            "E_INDEX_PREDICATE",
            "partial unique index option predicates require a field path",
        )),
        _ => Err(Error::new(
            "E_INDEX_PREDICATE",
            "partial unique index predicates only support field equality, option presence, and '&&'",
        )),
    }
}

fn collect_atoms(
    catalog: &Catalog,
    fields: &[Column],
    expression: &BoolExpression,
    atoms: &mut Vec<IndexPredicateAtom>,
) -> Result<()> {
    match expression {
        BoolExpression::And(left, right) => {
            collect_atoms(catalog, fields, left, atoms)?;
            collect_atoms(catalog, fields, right, atoms)
        }
        BoolExpression::Compare {
            left: ScalarExpression::Reference(column),
            op: CmpOp::Eq,
            right: ScalarExpression::Literal(value),
            operand_type: Some(value_type),
        } => {
            let field_path = catalog.field_path_ids(fields, column)?;
            if matches!(value, Value::Option(None)) {
                atoms.push(IndexPredicateAtom::IsNone {
                    column: column.clone(),
                    field_path,
                });
            } else {
                atoms.push(IndexPredicateAtom::Equal {
                    column: column.clone(),
                    field_path,
                    value_type: value_type.clone(),
                    value: value.clone(),
                });
            }
            Ok(())
        }
        BoolExpression::IsNone(ScalarExpression::Reference(column)) => {
            atoms.push(IndexPredicateAtom::IsNone {
                column: column.clone(),
                field_path: catalog.field_path_ids(fields, column)?,
            });
            Ok(())
        }
        BoolExpression::IsSome(ScalarExpression::Reference(column)) => {
            atoms.push(IndexPredicateAtom::IsSome {
                column: column.clone(),
                field_path: catalog.field_path_ids(fields, column)?,
            });
            Ok(())
        }
        _ => Err(Error::new(
            "E_INDEX_PREDICATE",
            "partial unique index predicate did not normalize to a supported atom",
        )),
    }
}

fn normalize(mut atoms: Vec<IndexPredicateAtom>) -> Result<IndexPredicate> {
    atoms.sort_by(|left, right| {
        left.field_path()
            .cmp(right.field_path())
            .then_with(|| atom_payload_key(left).cmp(&atom_payload_key(right)))
    });
    atoms.dedup_by(|left, right| equivalent(left, right));
    for (offset, left) in atoms.iter().enumerate() {
        for right in &atoms[offset + 1..] {
            if left.field_path() != right.field_path() {
                break;
            }
            if conflicts(left, right) {
                return Err(Error::new(
                    "E_INDEX_PREDICATE_CONTRADICTION",
                    format!(
                        "partial unique index predicate has conflicting conditions for field '{}'",
                        left.column()
                    ),
                ));
            }
        }
    }
    Ok(IndexPredicate { atoms })
}

fn equivalent(left: &IndexPredicateAtom, right: &IndexPredicateAtom) -> bool {
    if left.field_path() != right.field_path() {
        return false;
    }
    match (left, right) {
        (
            IndexPredicateAtom::Equal { value: left, .. },
            IndexPredicateAtom::Equal { value: right, .. },
        ) => left.cmp_eq(right),
        (IndexPredicateAtom::IsNone { .. }, IndexPredicateAtom::IsNone { .. })
        | (IndexPredicateAtom::IsSome { .. }, IndexPredicateAtom::IsSome { .. }) => true,
        _ => false,
    }
}

fn atom_payload_key(atom: &IndexPredicateAtom) -> Vec<u8> {
    #[derive(Serialize)]
    struct Key<'a> {
        operator: u8,
        value_type: Option<&'a ScalarType>,
        value: Option<&'a Value>,
    }

    let key = match atom {
        IndexPredicateAtom::Equal {
            value_type, value, ..
        } => Key {
            operator: 0,
            value_type: Some(value_type),
            value: Some(value),
        },
        IndexPredicateAtom::IsNone { .. } => Key {
            operator: 1,
            value_type: None,
            value: None,
        },
        IndexPredicateAtom::IsSome { .. } => Key {
            operator: 2,
            value_type: None,
            value: None,
        },
    };
    serde_json::to_vec(&key)
        .expect("serializing a bound index predicate atom key to memory cannot fail")
}

fn conflicts(left: &IndexPredicateAtom, right: &IndexPredicateAtom) -> bool {
    match (left, right) {
        (
            IndexPredicateAtom::Equal { value: left, .. },
            IndexPredicateAtom::Equal { value: right, .. },
        ) => !left.cmp_eq(right),
        (IndexPredicateAtom::IsNone { .. }, IndexPredicateAtom::IsSome { .. })
        | (IndexPredicateAtom::IsSome { .. }, IndexPredicateAtom::IsNone { .. })
        | (IndexPredicateAtom::IsNone { .. }, IndexPredicateAtom::Equal { .. })
        | (IndexPredicateAtom::Equal { .. }, IndexPredicateAtom::IsNone { .. }) => true,
        _ => false,
    }
}
