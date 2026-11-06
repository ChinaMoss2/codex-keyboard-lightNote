"""纯状态测试，不写入键盘或运行状态文件。"""
import unittest
from state_bridge import reduce_state


class StateTests(unittest.TestCase):
    def test_missing_stop_completion_reconciliation(self):
        from agent_completion import reconcile
        class Reader:
            def latest(self, agent):
                return ('task_complete', 'old', 10)
        s={'p':{'state':'running','agents':{
            'finished':{'turn':'old','updated':1},
            'resumed':{'turn':'new','updated':11}}}}
        reconcile(s,Reader())
        self.assertEqual(set(s['p']['agents']),{'resumed'})
        self.assertEqual(s['p']['state'],'running')

    def test_parent_child_lifecycle(self):
        sessions = {}
        def emit(event, turn='parent', agent=None, now=1):
            nonlocal sessions
            data = dict(session_id='p', turn_id=turn, hook_event_name=event)
            if agent:
                data['agent_id'] = agent
            sessions, selected = reduce_state(sessions, data, now, None)
            return selected
        self.assertEqual(emit('UserPromptSubmit'), 'running')
        self.assertEqual(emit('SubagentStart', 'c1', 'a'), 'running_subagents')
        emit('SubagentStart', 'c1', 'a')
        self.assertEqual(len(sessions['p']['agents']), 1)
        emit('SubagentStart', 'c2', 'b')
        emit('PreToolUse', 'c1', 'a')
        self.assertEqual(sessions['p']['turn'], 'parent')
        self.assertEqual(emit('PermissionRequest', 'c1', 'a'), 'waiting')
        self.assertEqual(emit('PostToolUse', 'c1', 'a'), 'running_subagents')
        self.assertEqual(emit('SubagentStop', 'c1', 'a'), 'running_subagents')
        self.assertEqual(emit('SubagentStop', 'c1', 'a'), 'running_subagents')
        self.assertEqual(emit('Stop'), 'running_subagents')
        self.assertEqual(emit('SubagentStop', 'c2', 'b', now=10), 'done')
        self.assertEqual(emit('SessionEnd', now=11), 'done')
        _, state = reduce_state(sessions, None, 16, None)
        self.assertEqual(state, 'idle')

    def test_resume_and_stale_stop(self):
        s = {'p': {'state': 'running', 'updated': 1, 'turn': 'parent', 'owner': None}}
        for event, turn in [('SubagentStart','c1'), ('SubagentStop','c1'), ('PreToolUse','c2')]:
            s, state = reduce_state(s, dict(session_id='p', agent_id='a',
                                   turn_id=turn, hook_event_name=event), 2, None)
        self.assertEqual(state, 'running_subagents')
        s,state=reduce_state(s,dict(session_id='p',agent_id='a',turn_id='c1',
                                 hook_event_name='SubagentStop'),3,None)
        self.assertEqual(state,'running_subagents')
        s,state=reduce_state(s,dict(session_id='p',turn_id='parent',
                                 hook_event_name='Interrupt'),4,None)
        self.assertEqual((s,state),({},'idle'))

    def test_named_followup_before_child_tool(self):
        s={'p':{'state':'running','turn':'parent','updated':1,'owner':None}}
        events=[dict(hook_event_name='PostToolUse',tool_name='collaborationspawn_agent',
                     tool_response={'task_name':'worker'}),
                dict(hook_event_name='SubagentStart',agent_id='a',turn_id='c1'),
                dict(hook_event_name='SubagentStop',agent_id='a',turn_id='c1'),
                dict(hook_event_name='PostToolUse',tool_name='collaborationfollowup_task',
                     tool_input={'target':'worker','message':'不应保存'},tool_response='已提交')]
        for e in events:
            s,state=reduce_state(s,dict(session_id='p',**e),2,None)
        self.assertEqual(state,'running_subagents')
        self.assertEqual(set(s['p']['agents']),{'a'})
        self.assertNotIn('不应保存',str(s))
        self.assertEqual(s['p']['aliases'],{'worker':'a'})


if __name__ == '__main__':
    unittest.main()
