import json, os, pathlib, subprocess, tempfile
source=pathlib.Path('crates/pbox-cli/src/ansible-plugins').resolve()
with tempfile.TemporaryDirectory(prefix='pbox-callback-') as root:
    root=pathlib.Path(root)
    play=root/'test.yml'
    play.write_text('''- hosts: localhost
  gather_facts: false
  tasks:
    - name: Visible log
      ansible.builtin.debug:
        msg: hello from Ansible
    - name: Skip me
      ansible.builtin.debug:
        msg: unused
      when: false
    - name: Private log
      ansible.builtin.debug:
        msg: SECRET_SENTINEL
      no_log: true
    - name: Expected failure
      ansible.builtin.fail:
        msg: deliberate failure
''')
    captured=[]
    for output in ('default','minimal'):
        events=root/(output+'.jsonl')
        events.touch()
        env=dict(os.environ,ANSIBLE_CALLBACK_PLUGINS=str(source),ANSIBLE_CALLBACKS_ENABLED='pbox_progress',PBOX_EVENTS=str(events),ANSIBLE_STDOUT_CALLBACK=output)
        result=subprocess.run(['ansible-playbook','-i','localhost,','-c','local',str(play)],env=env,capture_output=True)
        assert result.returncode==2, result.stderr
        text=events.read_text()
        assert 'SECRET_SENTINEL' not in text
        rows=[json.loads(line) for line in text.splitlines()]
        assert any(r['kind']=='log' and r['text']=='hello from Ansible' for r in rows)
        assert any(r['kind']=='error' and r['text']=='deliberate failure' for r in rows)
        assert any(r['kind']=='result' and r['skipped'] for r in rows)
        captured.append(rows)
    assert captured[0]==captured[1]
    print('PASS: default/minimal stdout produce identical events; logs, skips, failures and no_log verified')
