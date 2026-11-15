---
name: cron
description: Scheduled task management - create, query, update scheduled tasks to automatically execute operations at specified times.
---

# Scheduled Task Skill

Manage scheduled tasks for the current conversation with the bundled
agent-facing config CLI.

## Rules

1. Each conversation can have at most one scheduled task.
2. Always query existing tasks before creating or updating.
3. Do not ask for extra confirmation after the user has already requested the scheduling change.
4. Never pass, inline, export, echo, or set any `AIONUI_...` environment variable.
5. Commands must directly call the bundled helper: `"$AIONUI_HELPER_BIN" config cron current ...` in Bash, or `& $env:AIONUI_HELPER_BIN config cron current ...` in PowerShell.
6. Pass create and update payloads directly through stdin using a Bash heredoc or a PowerShell single-quoted here-string. Do not write payload JSON files to disk. In PowerShell, set UTF-8 encoding before piping JSON, as shown below.
7. Put `job_id` in the update JSON payload, not in a command flag.
8. After a successful create or update, send one short final confirmation that a normal user can understand. Include the task name and schedule description. Do not show internal ids such as `cron_...`.
9. After creating or updating, query the task again and compare its name, schedule description, and message with the intended text, including non-ASCII characters and line breaks. Repair a mismatched task once by updating its existing id, then read it back again; do not create another task. If the CLI fails or the text still differs, report the failure in normal prose and do not claim success.

## Workflow

1. Run `config cron current list` through the bundled helper, using the active-shell syntax below.
2. If the returned `data` array is empty, create the task with `config cron current create` and a stdin payload.
3. If one task exists and the user wants to change it, update that task with `config cron current update` and a stdin payload.
4. If a task already exists and the user is asking for a different additional task, ask how they want to handle the existing task.
5. Run the list command again and verify the saved task's `name`, `schedule.description`, and `target.payload.text` against the intended payload. Report success or failure in normal prose, following the final confirmation rule above.

## Payload

Create payload:

- `name`: Short descriptive name.
- `schedule`: Standard 5-field cron expression.
- `schedule_description`: Human-readable schedule.
- `message`: Complete, self-contained instruction sent to the AI when the task runs.

Update payload uses the same fields and also requires:

- `job_id`: The id from the existing task returned by the list command.

The `message` must tell the AI exactly what to do when the task fires. It should
not merely restate the user's scheduling request.

| User says                         | Bad message              | Good message                                                                             |
| --------------------------------- | ------------------------ | ---------------------------------------------------------------------------------------- |
| "Send me hello every day at 10am" | Send me hello            | Reply with exactly: Hello!                                                               |
| "Remind me to drink water daily"  | Remind me to drink water | Reply with a friendly reminder to drink water.                                           |
| "Summarize AI news every Monday"  | Summarize AI news        | Search for the latest AI news from this week and produce a concise bullet-point summary. |

## Examples

For real tasks, replace the sample task name, schedule description, and message
with text in the user's language. Use the syntax for the active shell.

### Bash

Query:

```bash
"$AIONUI_HELPER_BIN" config cron current list
```

Create:

```bash
"$AIONUI_HELPER_BIN" config cron current create <<'JSON'
{
  "name": "Weekly Meeting Reminder",
  "schedule": "0 9 * * MON",
  "schedule_description": "Every Monday at 9:00 AM",
  "message": "Reply with a short weekly meeting reminder that includes the current date and time."
}
JSON
```

Update:

```bash
"$AIONUI_HELPER_BIN" config cron current update <<'JSON'
{
  "job_id": "cron_123",
  "name": "Daily Summary",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "Weekdays at 6:00 PM",
  "message": "Review today's conversation context and produce a concise end-of-day summary."
}
JSON
```

### PowerShell

Windows PowerShell 5.1 defaults to ASCII when piping text to native programs,
which replaces Chinese and emoji with question marks before the helper receives
them. Set `$OutputEncoding` to UTF-8 before each create or update pipeline.
Set `[Console]::OutputEncoding` to UTF-8 when reading the helper's JSON output.
Repeat these settings in each new PowerShell invocation; changing the console
code page alone does not set the pipeline encoding. Do not change shell profiles.

Query:

```powershell
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
& $env:AIONUI_HELPER_BIN config cron current list
```

Create only after the query returns an empty `data` array:

```powershell
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
@'
{
  "name": "每日提醒 ✅",
  "schedule": "0 9 * * *",
  "schedule_description": "每天上午 9:00",
  "message": "请回复：你好，世界！\n记得喝水。"
}
'@ | & $env:AIONUI_HELPER_BIN config cron current create
```

Update with the existing id returned by the query:

```powershell
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
@'
{
  "job_id": "cron_123",
  "name": "每日总结 ✅",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "工作日下午 6:00",
  "message": "回顾今天的会话并给出简短总结。\n保留需要继续处理的事项。"
}
'@ | & $env:AIONUI_HELPER_BIN config cron current update
```

Keep the closing `'@` at the start of its line. The single-quoted here-string
keeps JSON quotes, backslashes, and literal `$` characters intact. After either
write, run the query again and verify the saved text before confirming success.

### Multiline messages

Multiline message example:

```json
{
  "name": "Daily Summary",
  "schedule": "0 9 * * *",
  "schedule_description": "Every day at 9:00 AM",
  "message": "First paragraph.\nSecond paragraph.\nThird paragraph."
}
```

## Cron Expression

Format: `minute hour day-of-month month day-of-week`.

Example: `0 9 * * MON-FRI` means weekdays at 9:00 AM.

Use only standard cron fields and ranges supported by the backend parser. Do
not use Quartz-style extensions such as `L`, `L-N`, `W`, `LW`, `#`, or `?`.
