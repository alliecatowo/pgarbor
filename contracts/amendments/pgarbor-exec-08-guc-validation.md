# exec-08: validation of stored GUC settings

Decision: ACCEPTED by central integration (2026-10-15).

The frozen `GucAccess` trait needs a side-effect-free `check_setting(name, value)`
method. ALTER ROLE/DATABASE/FUNCTION SET must validate before storing a setting;
calling `set` would change the current session. The production GucTable adapter
uses its existing normalization and validation hooks without assignment. Every
adapter must implement this method. NoGuc has no settings and rejects names with
42704; the executor routing test adapter delegates to it. This prevents an
unleased unsupported marker in a frozen context file and avoids unchecked
acceptance by default. No dependency direction changes.

Validation: the API session regression checks unknown names, invalid values and
valid stored role/function settings. Existing contract files remain unchanged.
