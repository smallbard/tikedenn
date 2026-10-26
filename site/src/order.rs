//! Ordre de traitement des tâches, commun au tableau et aux agents
//! (`claim_next`), pour que le tableau montre ce que les agents feront.
//!
//! - **Héritage de priorité** : une tâche prend la plus haute priorité des
//!   tâches qui attendent après elle (transitivement), sinon une tâche urgente
//!   resterait bloquée derrière une dépendance de priorité basse. Les
//!   brouillons ne transmettent pas leur priorité (ils ne sont pas prêts).
//! - **Plan d'une colonne** (todo, et draft de la même façon) : d'abord les
//!   tâches prêtes (dépendances terminées), dans l'ordre où les agents les
//!   prendront (priorité effective, puis la plus ancienne) ; puis celles qui
//!   attendent, triées topologiquement : une tâche vient toujours après ses
//!   dépendances.

use rusqlite::Connection;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

struct Node {
    status: String,
    priority: i64,
    /// Dépendances pas encore terminées.
    deps: Vec<i64>,
    /// Tâches non terminées qui dépendent de celle-ci.
    dependents: Vec<i64>,
}

/// Priorité effective d'une tâche, et la tâche dont elle l'hérite le cas
/// échéant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Effective {
    pub priority: i64,
    pub inherited_from: Option<i64>,
}

/// Graphe des tâches non terminées et de leurs dépendances non terminées.
pub struct Graph {
    nodes: HashMap<i64, Node>,
}

impl Graph {
    pub fn load(conn: &Connection) -> rusqlite::Result<Graph> {
        let mut nodes: HashMap<i64, Node> = conn
            .prepare("SELECT id, status, priority FROM tasks WHERE status != 'done'")?
            .query_map([], |r| {
                Ok((r.get(0)?, Node { status: r.get(1)?, priority: r.get(2)?, deps: vec![], dependents: vec![] }))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let edges: Vec<(i64, i64)> = conn
            .prepare("SELECT task_id, depends_on FROM task_deps")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (task, dep) in edges {
            // Dépendance terminée (absente du graphe) : plus de contrainte.
            if nodes.contains_key(&task) && nodes.contains_key(&dep) {
                nodes.get_mut(&task).unwrap().deps.push(dep);
                nodes.get_mut(&dep).unwrap().dependents.push(task);
            }
        }
        Ok(Graph { nodes })
    }

    /// Vrai si toutes les dépendances de la tâche sont terminées.
    pub fn is_ready(&self, id: i64) -> bool {
        self.nodes.get(&id).is_none_or(|n| n.deps.is_empty())
    }

    /// Priorités effectives (héritage transitif depuis les tâches dépendantes).
    pub fn effective(&self) -> HashMap<i64, Effective> {
        let mut memo = HashMap::new();
        for &id in self.nodes.keys() {
            self.effective_of(id, &mut memo, &mut vec![]);
        }
        memo
    }

    fn effective_of(&self, id: i64, memo: &mut HashMap<i64, Effective>, path: &mut Vec<i64>) -> Effective {
        if let Some(e) = memo.get(&id) {
            return *e;
        }
        let node = &self.nodes[&id];
        let mut best = Effective { priority: node.priority, inherited_from: None };
        // Les cycles sont refusés à la saisie ; `path` évite de boucler malgré tout.
        path.push(id);
        for &d in &node.dependents {
            if path.contains(&d) || self.nodes[&d].status == "draft" {
                continue;
            }
            let e = self.effective_of(d, memo, path);
            if e.priority > best.priority {
                best = Effective { priority: e.priority, inherited_from: Some(e.inherited_from.unwrap_or(d)) };
            }
        }
        path.pop();
        memo.insert(id, best);
        best
    }

    /// Ordre des tâches d'une colonne (`todo` : ordre de traitement des
    /// agents ; voir l'en-tête du module).
    pub fn plan(&self, status: &str, eff: &HashMap<i64, Effective>) -> Vec<i64> {
        let in_col = |id: &i64| self.nodes.get(id).is_some_and(|n| n.status == status);
        // Seules les dépendances internes à la colonne contraignent l'ordre ;
        // une dépendance ailleurs (déjà en cours…) n'impose rien ici.
        let mut waiting_on: HashMap<i64, usize> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.status == status)
            .map(|(&id, n)| (id, n.deps.iter().filter(|d| in_col(d)).count()))
            .collect();
        let key = |id: i64| (self.is_ready(id), eff[&id].priority, Reverse(id));
        let mut heap: BinaryHeap<_> = waiting_on.iter().filter(|(_, &n)| n == 0).map(|(&id, _)| key(id)).collect();
        let mut plan = Vec::with_capacity(waiting_on.len());
        while let Some((_, _, Reverse(id))) = heap.pop() {
            plan.push(id);
            for d in &self.nodes[&id].dependents {
                if let Some(n) = waiting_on.get_mut(d) {
                    *n -= 1;
                    if *n == 0 {
                        heap.push(key(*d));
                    }
                }
            }
        }
        plan
    }
}

/// Trie des tâches candidates (ids) dans l'ordre où un agent doit les
/// prendre : priorité effective décroissante, puis la plus ancienne.
pub fn sort_for_agents(conn: &Connection, ids: &mut [i64]) -> rusqlite::Result<()> {
    let eff = Graph::load(conn)?.effective();
    let prio = |id: &i64| eff.get(id).map_or(0, |e| e.priority);
    ids.sort_by_key(|id| (Reverse(prio(id)), *id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(sql: &str) -> (Connection, Graph) {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_app_schema(&conn).unwrap();
        conn.execute_batch(sql).unwrap();
        let g = Graph::load(&conn).unwrap();
        (conn, g)
    }

    #[test]
    fn priority_is_inherited_transitively_but_not_from_drafts() {
        // 3 (urgent) → 2 → 1 (low) ; 4 (draft, urgent) → 5 (low) ; 6 (done) ignorée.
        let (_c, g) = graph(
            "INSERT INTO tasks (id, title, status, priority) VALUES
                (1, 'a', 'todo', 0), (2, 'b', 'todo', 1), (3, 'c', 'todo', 3),
                (4, 'd', 'draft', 3), (5, 'e', 'todo', 0), (6, 'f', 'done', 3);
             INSERT INTO task_deps VALUES (3, 2), (2, 1), (4, 5), (6, 5);",
        );
        let eff = g.effective();
        assert_eq!(eff[&1], Effective { priority: 3, inherited_from: Some(3) });
        assert_eq!(eff[&2], Effective { priority: 3, inherited_from: Some(3) });
        assert_eq!(eff[&3], Effective { priority: 3, inherited_from: None });
        assert_eq!(eff[&5], Effective { priority: 0, inherited_from: None });
    }

    #[test]
    fn plan_puts_ready_tasks_first_then_respects_dependencies() {
        // 1 low prête ; 2 urgente dépend de 3 (normal, prête) ; 4 high dépend
        // de 5 (en cours) ; 6 normal prête, plus récente que 3.
        let (_c, g) = graph(
            "INSERT INTO tasks (id, title, status, priority) VALUES
                (1, 'a', 'todo', 0), (2, 'b', 'todo', 3), (3, 'c', 'todo', 1),
                (4, 'd', 'todo', 2), (5, 'e', 'progress', 1), (6, 'f', 'todo', 1);
             INSERT INTO task_deps VALUES (2, 3), (4, 5);",
        );
        let eff = g.effective();
        // Prêtes : 3 (hérite d'urgent), 6, 1 ; puis en attente : 2 (urgente), 4 (high).
        assert_eq!(g.plan("todo", &eff), vec![3, 6, 1, 2, 4]);
        assert!(g.is_ready(3) && !g.is_ready(2) && !g.is_ready(4));
    }

    #[test]
    fn drafts_are_planned_like_todo() {
        // 1 draft low prête ; 2 draft urgente dépend de 3 (draft) ; 4 draft
        // dépend de 5 (todo) ; 6 todo ignorée.
        let (_c, g) = graph(
            "INSERT INTO tasks (id, title, status, priority) VALUES
                (1, 'a', 'draft', 0), (2, 'b', 'draft', 3), (3, 'c', 'draft', 1),
                (4, 'd', 'draft', 2), (5, 'e', 'todo', 1), (6, 'f', 'todo', 3);
             INSERT INTO task_deps VALUES (2, 3), (4, 5);",
        );
        let eff = g.effective();
        assert_eq!(g.plan("draft", &eff), vec![3, 1, 2, 4]);
    }

    #[test]
    fn agents_take_the_blocker_of_an_urgent_task_first() {
        let (conn, _g) = graph(
            "INSERT INTO tasks (id, title, status, priority) VALUES
                (1, 'a', 'todo', 1), (2, 'b', 'todo', 0), (3, 'c', 'todo', 3);
             INSERT INTO task_deps VALUES (3, 2);",
        );
        let mut ids = vec![1, 2];
        sort_for_agents(&conn, &mut ids).unwrap();
        assert_eq!(ids, vec![2, 1]);
    }
}
