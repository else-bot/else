# Sidebar categories

## Sub-features

- Create and rename categories; reject duplicate names without regard to case.
- Choose bots during category creation, with search, an Unassigned filter, and a live sidebar preview.
- Assign and unassign a bot while keeping its selected conversation.
- Delete a category and keep its bots under Unassigned.
- Drag a bot between sidebar groups without changing its selected conversation.
- Reorder categories by dragging their headers, and collapse a group.
- Push changes to connected clients and preserve grouping over restart.

## How to get to it (user POV)

Use Ctrl+Shift+G or the icon-only folder button in the sidebar footer.
Move a bot with Ctrl+Shift+M (a **Move {bot}** dialog: Tab to a category,
Space to choose, Enter moves), its row's **Move to** context menu, or by
dragging it. Right-click a category header for **Rename category…** and
**Delete category…**; left-click collapses it. The main bot stays pinned
above every group and cannot be moved. The New bot dialog shows a
**Category** select once a category exists.

## Driving it with verify-else

The default `categories` lane creates Family, rejects FAMILY with 409,
assigns its bot using `PATCH /v1/bots/{id}/category`, renames to Home and
checks the sidebar payload. Deleting the category must remove its grouping
and preserve the bot with `category_id:null`. Run
`verify.py --tests categories ws` for persistence, order, and multi-client pushes.

The native helper seeds only the main bot, which cannot be moved. Create
ordinary bots first (Ctrl+Shift+N, or `--probe native --method POST
--path /v1/bots --expected 201` with a fresh `request_id`). Then follow the
native Ctrl+Shift+G journey in SKILL.md. The focused category-name placeholder is
`e.g. Everyday`; Enter in that field submits. Search for a bot, select it, and clear the search.
Verify that the selection stays in the preview and through the Unassigned filter.
Enter in the search field must leave the dialog open. Create the category and check that its
selected bots have the new category ID in `/v1/bots`. Also create an empty category.
Verify `Verification category` appears in the sidebar and `--probe native --path /v1/categories`. For picker
changes, also move via both mouse and Ctrl+Shift+M keyboard selection, check
the bot ID/category in `/v1/bots`, and verify the selected bot and draft stay.
For drag changes, capture the source group, drag over a different group,
drop, and check the new group and `/v1/bots` category UUID. Exercise both
Unassigned and two named categories. A picker move cannot prove dragging.
For create-dialog changes, also create a bot with an initial category. For
deletion changes, capture the confirmation ("Bots move to Unassigned") and
the bots' subsequent Unassigned placement.

## Gotchas

- A native session needs completed onboarding; the helper supplies that fixture.
- Do not replace a failed keyboard gesture with API creation and call it passed.
- The Unassigned group is hidden while empty, so dropping there needs another
  unassigned ordinary bot.
- The dialog's sidebar preview needs a window at least 760 px wide.
- A bot waiting on the owner also appears in the "Needs you" tray, so it
  can show twice in a screenshot.
- The automated lane proves sidebar data, not its ordering or rendered layout.
