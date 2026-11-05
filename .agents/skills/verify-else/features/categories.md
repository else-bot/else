# Sidebar categories

## Sub-features

- Create and rename categories; reject duplicate names without regard to case.
- Choose bots during category creation, with search, an Unassigned filter, and a live sidebar preview.
- Assign and unassign a bot while keeping its selected conversation.
- Delete a category and keep its bots under Unassigned.
- Drag a bot between sidebar groups without changing its selected conversation.
- Push changes to connected clients and preserve grouping over restart.

## How to get to it (user POV)

Use New category or Ctrl+Shift+G. Move the selected bot with its Category
control or Ctrl+Shift+M. Use the category's sidebar controls to rename/delete.
The New bot dialog can assign the bot to a category at creation.

## Driving it with verify-else

The default `categories` lane creates Family, rejects FAMILY with 409,
assigns its bot using `PATCH /v1/bots/{id}/category`, renames to Home and
checks the sidebar payload. Deleting the category must remove its grouping
and preserve the bot with `category_id:null`. Run
`verify.py --tests categories ws` for persistence and multi-client pushes.

Follow the native Ctrl+Shift+G journey in SKILL.md. The focused category-name placeholder is
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
deletion changes, capture the confirmation that bots are kept and their
subsequent Unassigned placement.

## Gotchas

- A native session needs completed onboarding; the helper supplies that fixture.
- Do not replace a failed keyboard gesture with API creation and call it passed.
- The automated lane proves sidebar data, not its ordering or rendered layout.
