<!-- À copier dans le AGENTS.md / CLAUDE.md d'un dépôt dont les tâches viennent de Tikedenn. -->
## Tikedenn

Work on this repository is queued on the Tikedenn board (kanban shared with humans), through the `tkd` CLI.
- Take work with `tkd next`, read it with `tkd show <id>` (spec, acceptance criteria, review feedback in the log).
- Work on the task branch (`task/<id>-…`), never on the base branch; commit, push the branch.
- Finish with `tkd submit <id> -m "<what changed, how it was tested, open questions>"`: a human reviews the diff,
  then the deploy worker merges and publishes. If blocked: `tkd submit <id> -m "BLOCKED: <question>"`.
- Full protocol: `tkd install-skill` installs the `tikedenn` skill for Claude Code and pi.
