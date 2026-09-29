#!/usr/bin/env python3
"""Prove the constraints in 0064_ai_agents.sql are load-bearing.

Every case is an INSERT that MUST fail. A check constraint that never fires is a comment, and
a constraint that fires for the *wrong reason* (a missing foreign key rather than the rule under
test) is worse than no constraint: the test is green and the rule is unproven. So each case
asserts on the constraint NAME postgres names, not merely on "it errored".

Usage: python3 scripts/qa/agent-migration-checks.py [database]
"""
import os
import subprocess
import sys
import uuid

DB = sys.argv[1] if len(sys.argv) > 1 else "omnion_w7_mig"
PSQL = ["psql", "-h", "127.0.0.1", "-p", "5433", "-U", "omnion", "-d", DB, "-v", "ON_ERROR_STOP=1"]
ENV = {**os.environ, "PGPASSWORD": "omnion"}


def sql(statement: str) -> tuple[int, str]:
    done = subprocess.run(PSQL + ["-tAc", statement], capture_output=True, text=True, env=ENV)
    return done.returncode, (done.stderr or done.stdout).strip()


def main() -> int:
    org = str(uuid.uuid4())
    rc, msg = sql(f"insert into organizations (id, name, slug) values ('{org}', 'Acme', 'acme-{org[:8]}')")
    if rc != 0:
        print(f"could not create the fixture organization: {msg}")
        return 1
    sql(f"insert into ai_agents (organization_id, key, name) values ('{org}', 'reporter', 'Reporter')")

    # (label, statement, the constraint name the error MUST name)
    cases = [
        ("a key with a space is refused", f"insert into ai_agents (organization_id,key,name) values ('{org}','bad key','X')", "ai_agents_key_format"),
        ("an upper-case key is refused", f"insert into ai_agents (organization_id,key,name) values ('{org}','Bad','X')", "ai_agents_key_format"),
        ("a duplicate key in one organization is refused", f"insert into ai_agents (organization_id,key,name) values ('{org}','reporter','Dup')", "ai_agents_org_key_uidx"),
        ("zero max steps is refused", f"insert into ai_agents (organization_id,key,name,max_steps) values ('{org}','s0','X',0)", "ai_agents_max_steps_range"),
        ("max steps above the ceiling is refused", f"insert into ai_agents (organization_id,key,name,max_steps) values ('{org}','s51','X',51)", "ai_agents_max_steps_range"),
        ("a deadline under the floor is refused", f"insert into ai_agents (organization_id,key,name,deadline_seconds) values ('{org}','d29','X',29)", "ai_agents_deadline_range"),
        ("a token budget under the floor is refused", f"insert into ai_agents (organization_id,key,name,token_budget) values ('{org}','t999','X',999)", "ai_agents_token_budget_range"),
        ("a temperature above 1 is refused", f"insert into ai_agents (organization_id,key,name,temperature) values ('{org}','t15','X',1.5)", "ai_agents_temperature_range"),
        ("tools as an object rather than a list is refused", f"insert into ai_agents (organization_id,key,name,tools) values ('{org}','tobj','X','{{}}'::jsonb)", "ai_agents_tools_is_array"),
        ("approvals as an object rather than a list is refused", f"insert into ai_agents (organization_id,key,name,approvals) values ('{org}','aobj','X','{{}}'::jsonb)", "ai_agents_approvals_is_array"),
        ("an unknown memory scope is refused", f"insert into ai_agents (organization_id,key,name,memory_scope) values ('{org}','ms','X','galaxy')", "ai_agents_memory_scope_known"),
        ("a name over 80 characters is refused", f"insert into ai_agents (organization_id,key,name) values ('{org}','n81','{ 'a' * 81 }')", "ai_agents_name_length"),
        ("a description over 400 characters is refused", f"insert into ai_agents (organization_id,key,name,description) values ('{org}','d401','X','{ 'c' * 401 }')", "ai_agents_description_length"),
        ("a system prompt over 8000 characters is refused", f"insert into ai_agents (organization_id,key,name,system_prompt) values ('{org}','sp','X','{ 'b' * 8001 }')", "ai_agents_system_prompt_length"),
        ("an unknown run trigger is refused", f"insert into ai_runs (organization_id,trigger,goal) values ('{org}','telepathy','g')", "ai_runs_trigger_known"),
        ("an unknown run status is refused", f"insert into ai_runs (organization_id,status,goal) values ('{org}','vibing','g')", "ai_runs_status_known"),
        ("an unknown stop reason is refused", f"insert into ai_runs (organization_id,status,stop_reason,goal) values ('{org}','completed','vibes','g')", "ai_runs_stop_reason_known"),
        ("a completed run with no stop reason is refused", f"insert into ai_runs (organization_id,status,goal) values ('{org}','completed','g')", "ai_runs_completed_names_a_reason"),
        ("a still-running run claiming a stop reason is refused", f"insert into ai_runs (organization_id,status,stop_reason,goal) values ('{org}','running','final_answer','g')", "ai_runs_reason_implies_finished"),
        ("a finished_at on a run that is still running is refused", f"insert into ai_runs (organization_id,status,finished_at,stop_reason,goal) values ('{org}','running',now(),'error','g')", "ai_runs_finished_has_stamp"),
        ("an empty goal is refused", f"insert into ai_runs (organization_id,goal) values ('{org}','')", "ai_runs_goal_length"),
        ("a negative current_step is refused", f"insert into ai_runs (organization_id,goal,current_step) values ('{org}','g',-1)", "ai_runs_current_step_non_negative"),
        ("a step of an unknown kind is refused", "insert into ai_run_steps (run_id,step_no,kind) values (gen_random_uuid(),1,'vibe')", "ai_run_steps_kind_known"),
        ("step number zero is refused", "insert into ai_run_steps (run_id,step_no,kind) values (gen_random_uuid(),0,'message')", "ai_run_steps_step_no_positive"),
        ("a tool_call step with no tool named is refused", "insert into ai_run_steps (run_id,step_no,kind) values (gen_random_uuid(),1,'tool_call')", "ai_run_steps_tool_only_for_tool_kinds"),
        ("a tool_result step with no tool named is refused", "insert into ai_run_steps (run_id,step_no,kind) values (gen_random_uuid(),1,'tool_result')", "ai_run_steps_tool_only_for_tool_kinds"),
    ]

    failures = 0
    for label, statement, expected in cases:
        rc, msg = sql(statement)
        named = expected in msg
        if rc == 0:
            print(f"  FAIL  {label} — the row was accepted")
            failures += 1
        elif not named:
            print(f"  FAIL  {label} — refused for the wrong reason: {msg.splitlines()[0][:110]}")
            failures += 1
        else:
            print(f"  ok    {label}")

    # The positive control: a row that satisfies every rule must still be accepted, or the
    # constraints are refusing everything and the cases above prove nothing.
    rc, msg = sql(
        f"insert into ai_agents (organization_id,key,name,system_prompt,temperature,max_steps,"
        f"deadline_seconds,token_budget,tools,approvals,memory_scope)"
        f" values ('{org}','good_one','Good','prompt',0.20,8,300,200000,'[\"read\"]'::jsonb,"
        f"'[\"write\"]'::jsonb,'site')"
    )
    if rc != 0:
        print(f"  FAIL  a valid agent is refused: {msg.splitlines()[0][:110]}")
        failures += 1
    else:
        print("  ok    a valid agent is accepted")

    print(f"\n{len(cases) + 1 - failures}/{len(cases) + 1} migration rules hold")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
