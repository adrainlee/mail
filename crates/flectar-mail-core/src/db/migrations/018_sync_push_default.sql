-- Siftmail: `syncSchedule.idlePush` became optional (missing = protocol
-- default: Gmail API / JMAP keep push, IMAP polls quietly). Builds before
-- this always wrote `false` without the user choosing it (no UI existed),
-- which would silently turn off Gmail/JMAP push. Drop that implicit value.
UPDATE accounts
   SET settings_json = json_remove(settings_json, '$.syncSchedule.idlePush')
 WHERE json_valid(settings_json)
   AND json_type(settings_json, '$.syncSchedule.idlePush') = 'false';
