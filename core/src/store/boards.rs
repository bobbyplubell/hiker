//! Derived `board_cards` index queries. Mirrors `store::trails`: re-derived
//! from each board-doc's frontmatter on ingest (clear-by-board +
//! re-insert), cleared on board-doc delete, fail-loud on schema bump like
//! every other derived table. See `docs/kanban.md` §"Indexer integration".

use rusqlite::params;

use super::dto::{BoardCardRow, BoardContainingHit};
use super::error::Error;
use super::Store;

impl Store {
    /// Clear every row for `board_id`, then insert `rows`. The trail-doc
    /// walk path uses the same clear-then-reinsert shape so the derived
    /// table stays consistent with the board-doc's `hiker.columns` array
    /// (frontmatter is the source of truth).
    ///
    /// status: board-cards-derived-table
    pub fn replace_board_cards(
        &mut self,
        board_id: &str,
        rows: &[BoardCardRow],
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM board_cards WHERE board_id = ?1", params![board_id])?;
        for row in rows {
            tx.execute(
                "INSERT INTO board_cards
                   (board_id, board_path, card_note_id, card_note_path, column_name, ordinal)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.board_id,
                    row.board_path,
                    row.card_note_id,
                    row.card_note_path,
                    row.column_name,
                    row.ordinal,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every row tied to `board_id`. Used on board-doc delete.
    ///
    /// status: board-cards-derived-table
    pub fn delete_board_cards_by_board(&mut self, board_id: &str) -> Result<usize, Error> {
        let n = self
            .conn
            .execute("DELETE FROM board_cards WHERE board_id = ?1", params![board_id])?;
        Ok(n)
    }

    /// Delete every row whose `board_path` matches — the board-doc-delete
    /// cleanup path keyed on the deleted path (the board id isn't known
    /// from the path alone at indexer delete time).
    ///
    /// status: board-cards-derived-table
    pub fn delete_board_cards_by_board_path(&mut self, board_path: &str) -> Result<usize, Error> {
        let n = self.conn.execute(
            "DELETE FROM board_cards WHERE board_path = ?1",
            params![board_path],
        )?;
        Ok(n)
    }

    /// Boards that contain a given note as a card. `note_id_or_path` is
    /// matched against both `card_note_id` and `card_note_path` so the
    /// caller doesn't have to pre-resolve. One hit per (board, column).
    ///
    /// status: board-cards-derived-table
    /// status: board-many-to-many
    pub fn boards_containing_note(
        &self,
        note_id_or_path: &str,
    ) -> Result<Vec<BoardContainingHit>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT board_id, board_path, column_name
             FROM board_cards
             WHERE card_note_id = ?1 OR card_note_path = ?1
             ORDER BY board_id, column_name",
        )?;
        let rows = stmt
            .query_map(params![note_id_or_path], |row| {
                Ok(BoardContainingHit {
                    board_id: row.get(0)?,
                    board_path: row.get(1)?,
                    column_name: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All card rows for a given board, ordered by column then ordinal.
    ///
    /// status: board-cards-derived-table
    pub fn cards_of(&self, board_id: &str) -> Result<Vec<BoardCardRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT board_id, board_path, card_note_id, card_note_path, column_name, ordinal
             FROM board_cards
             WHERE board_id = ?1
             ORDER BY column_name, ordinal",
        )?;
        let rows = stmt
            .query_map(params![board_id], |row| {
                Ok(BoardCardRow {
                    board_id: row.get(0)?,
                    board_path: row.get(1)?,
                    card_note_id: row.get(2)?,
                    card_note_path: row.get(3)?,
                    column_name: row.get(4)?,
                    ordinal: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Distinct board-doc paths known to the derived index — i.e. every
    /// board that currently has at least one card. Cheap one-query lookup
    /// the file tree uses to mark board rows + route their click to the
    /// board view. (A board with zero cards has no rows here and opens as a
    /// plain buffer until it gains a card — acceptable for v1.)
    ///
    /// status: board-cards-derived-table
    pub fn board_paths(&self) -> Result<Vec<String>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT board_path FROM board_cards ORDER BY board_path")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rewrite the `card_note_path` of every row pointing at `old_path` to
    /// `new_path`. Used by the auto-update-on-move path when a *referenced
    /// note* moves (case 1) so the derived table tracks the new path even
    /// before the next board-doc ingest.
    ///
    /// status: board-cards-derived-table
    pub fn rename_board_card_note_paths(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<usize, Error> {
        let n = self.conn.execute(
            "UPDATE board_cards SET card_note_path = ?1 WHERE card_note_path = ?2",
            params![new_path, old_path],
        )?;
        Ok(n)
    }

    /// Rewrite the `board_path` of every row for the board-doc that moved
    /// from `old_path` to `new_path` (case 2: the board-doc itself moved).
    /// Exact-match on `board_path`.
    ///
    /// status: board-cards-derived-table
    pub fn rename_board_card_paths_for_board(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<usize, Error> {
        let n = self.conn.execute(
            "UPDATE board_cards SET board_path = ?1 WHERE board_path = ?2",
            params![new_path, old_path],
        )?;
        Ok(n)
    }
}
