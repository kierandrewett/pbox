"""Versioned progress events; independent of Ansible's stdout callback."""
import json
import os
from ansible.plugins.callback import CallbackBase


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = 'aggregate'
    CALLBACK_NAME = 'pbox_progress'
    CALLBACK_NEEDS_ENABLED = True

    def emit(self, kind, **values):
        try:
            with open(os.environ['PBOX_EVENTS'], 'a', encoding='utf-8') as stream:
                stream.write(json.dumps(dict(version=1, kind=kind, **values)) + '\n')
        except (OSError, KeyError, TypeError):
            # Presentation must never change the result of an Ansible operation.
            pass

    def v2_playbook_on_task_start(self, task, is_conditional):
        self.emit('task', text=task.get_name())

    def v2_playbook_on_handler_task_start(self, task):
        self.emit('task', text=task.get_name())

    def result(self, result, success=True, skipped=False, ignored=False):
        data = result._result
        hidden = data.get('_ansible_no_log') or getattr(result._task, 'no_log', False)
        if hidden:
            message = 'Task output hidden by no_log'
        else:
            message = str(data.get('stderr') or data.get('msg') or '')
            for field in ('stdout', 'stderr', 'msg'):
                for line in str(data.get(field) or '').splitlines():
                    self.emit('log', text=line)
        if not success and not ignored:
            self.emit('error', text=message or 'Ansible task failed')
        self.emit('result', success=success, skipped=skipped)

    def v2_runner_on_ok(self, result):
        self.result(result)

    def v2_runner_on_failed(self, result, ignore_errors=False):
        self.result(result, success=False, ignored=ignore_errors)

    def v2_runner_on_unreachable(self, result):
        self.result(result, success=False)

    def v2_runner_on_skipped(self, result):
        self.result(result, skipped=True)

    def v2_runner_retry(self, result):
        self.emit('log', text='Retrying task')
