//! Single-pass reversal of a prefix expressed with standard list combinators.

use super::*;

impl Fuser<'_> {
    pub(super) fn reverse_prefix(&self, expression: &CExpr) -> Option<CExpr> {
        let append = self.prefix_call(expression, Comb::Append, 2)?;
        let reverse = self.prefix_call(&append[0], Comb::Reverse, 1)?;
        let take = self.prefix_call(&reverse[0], Comb::Take, 2)?;
        let drop = self.prefix_call(&append[1], Comb::Drop, 2)?;
        if !matches!(take[1].kind, K::Local(_))
            || !matches!(take[0].kind, K::Local(_) | K::Lit(Lit::Int(_)))
            || take != drop
        {
            return None;
        }
        Some(CExpr::new(
            K::Prim { op: Prim::ListReversePrefix, args: take.to_vec() },
            expression.ty.clone(),
        ))
    }

    fn prefix_call<'e>(
        &self,
        expression: &'e CExpr,
        expected: Comb,
        arity: usize,
    ) -> Option<&'e [CExpr]> {
        let (def, args) = call_target(expression)?;
        (self.defs.lookup(def) == Some((SeqKind::List, expected)) && args.len() == arity)
            .then_some(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lowered(body: &str) -> String {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), format!("module M\nlet f n xs ys = {body}\n"));
        crate::pretty_def(&fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("f")).body)
    }

    #[test]
    fn identical_operands_fuse() {
        assert!(
            lowered("List.append (List.reverse (List.take n xs)) (List.drop n xs)")
                .contains("listReversePrefix")
        );
    }

    #[test]
    fn different_sources_keep_the_original_operations() {
        assert!(
            !lowered("List.append (List.reverse (List.take n xs)) (List.drop n ys)")
                .contains("listReversePrefix")
        );
    }

    #[test]
    fn different_counts_keep_the_original_operations() {
        assert!(
            !lowered("List.append (List.reverse (List.take n xs)) (List.drop (n + 1) xs)")
                .contains("listReversePrefix")
        );
    }
}
