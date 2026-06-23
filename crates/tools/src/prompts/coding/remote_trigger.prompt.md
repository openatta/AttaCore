Manage remote agent triggers via the ClawPod bridge/CCR API.
Actions:
- list: List all configured remote triggers
- get: Get details of a specific trigger (requires trigger_id)
- create: Create a new remote trigger (requires body JSON with name, cron_expression, job_config)
- run: Execute a remote trigger immediately (requires trigger_id)
