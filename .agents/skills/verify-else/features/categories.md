# Sidebar categories

## Sub-features

- Create and rename categories; reject duplicate names without regard to case.
- Assign and unassign a bot while keeping its selected conversation.
- Delete a category and keep its bots under Unassigned.
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

Follow the native Ctrl+Shift+G journey in SKILL.md. The focused placeholder is
`Category name, e.g. Family`; Enter submits. Verify `Verification category`
appears in the sidebar and `--probe native --path /v1/categories`. For picker
changes, also move via both mouse and Ctrl+Shift+M keyboard selection, check
the bot ID/category in `/v1/bots`, and verify the selected bot and draft stay.
For create-dialog changes, also create a bot with an initial category. For
deletion changes, capture the confirmation that bots are kept and their
subsequent Unassigned placement.

## Gotchas

- A native session needs completed onboarding; the helper supplies that fixture.
- Do not replace a failed keyboard gesture with API creation and call it passed.
- The automated lane proves sidebar data, not its ordering or rendered layout.
