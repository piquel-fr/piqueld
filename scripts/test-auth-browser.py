#!/usr/bin/env python3
"""Real browser/CLI auth verification. Invoked by tests/auth_browser.rs.

Only Python's standard library is required. Chromium supplies a virtual CTAP2
resident-key authenticator with user verification; no real account is touched.
"""
import json
import os
import pathlib
import re
import socket
import subprocess
import time
import urllib.request
import urllib.error

ORIGIN = os.environ['AUTH_TEST_ORIGIN']
ROOT = pathlib.Path(__file__).resolve().parent.parent

class Browser:
    def __init__(self):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            self.port = sock.getsockname()[1]
        self.log = open(pathlib.Path(os.environ['AUTH_TEST_DIR']) / 'driver.log', 'w')
        self.driver = subprocess.Popen([os.environ['CHROMEDRIVER'], f'--port={self.port}'], stdout=self.log, stderr=self.log)
        self.session = None
        for _ in range(100):
            try:
                self.call('GET', '/status')
                break
            except (urllib.error.URLError, ConnectionError):
                time.sleep(.1)
        value = self.call('POST', '/session', {'capabilities': {'alwaysMatch': {
            'browserName': 'chrome', 'goog:chromeOptions': {
                'binary': os.environ['CHROMIUM'],
                'args': ['--headless=new', '--no-sandbox', '--disable-dev-shm-usage', '--no-proxy-server']
            }, 'goog:loggingPrefs': {'browser': 'ALL'}
        }}})
        self.session = value['sessionId']
        self.call('POST', self.path('/timeouts'), {'script': 30000})
        self.call('POST', self.path('/goog/cdp/execute'), {'cmd':'WebAuthn.enable','params':{}})
        self.authenticator = self.add_authenticator()
    def add_authenticator(self):
        return self.call('POST', self.path('/goog/cdp/execute'), {'cmd':'WebAuthn.addVirtualAuthenticator','params':{'options':{
            'protocol':'ctap2','transport':'internal','hasResidentKey':True,
            'hasUserVerification':True,'isUserVerified':True,'automaticPresenceSimulation':True
        }}})['authenticatorId']
    def path(self, suffix):
        return f'/session/{self.session}{suffix}'
    def call(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(f'http://127.0.0.1:{self.port}{path}', data=data, method=method, headers={'Content-Type':'application/json'})
        try:
            with urllib.request.urlopen(request, timeout=40) as response:
                result = json.load(response)['value']
        except urllib.error.HTTPError as error:
            raise AssertionError(error.read().decode()) from error
        if isinstance(result, dict) and 'error' in result:
            raise AssertionError(result)
        return result
    def js(self, script, *args):
        return self.call('POST', self.path('/execute/sync'), {'script':script, 'args':list(args)})
    def async_js(self, script, *args):
        return self.call('POST', self.path('/execute/async'), {'script':script, 'args':list(args)})
    def visit(self, url):
        self.call('POST', self.path('/url'), {'url':url})
    def wait(self, condition):
        for _ in range(200):
            if self.js(condition): return
            time.sleep(.1)
        raise AssertionError('Browser condition timed out: '+condition+'\n'+self.js('return document.body.innerText'))
    def click(self, text):
        self.js("const b=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()===arguments[0]); if(!b) throw new Error('missing button '+arguments[0]); b.click();", text)
    def screenshot(self, name):
        if directory := os.environ.get('AUTH_TEST_SCREENSHOTS'):
            import base64
            path=pathlib.Path(directory);path.mkdir(parents=True,exist_ok=True)
            (path/name).write_bytes(base64.b64decode(self.call('GET',self.path('/screenshot'))))
    def fill(self, label, value):
        self.js("const l=[...document.querySelectorAll('label')].find(l=>l.textContent.trim()===arguments[0]); if(!l) throw new Error('missing input '+arguments[0]);const e=l.querySelector('input');e.value=arguments[1];e.dispatchEvent(new Event('input',{bubbles:true}));", label, value)
    def api(self, path, body=None):
        return self.async_js("""const [path, body, done]=arguments; fetch('/api/v1/auth/'+path,body===null?{}:{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)}).then(async r=>done({status:r.status,body:await r.json()})).catch(e=>done({error:String(e)}));""",path,body)
    def ceremony(self, options, registration=False):
        return self.async_js("""const [options, registration, done]=arguments;
            const publicKey=registration ? PublicKeyCredential.parseCreationOptionsFromJSON(options.publicKey) : PublicKeyCredential.parseRequestOptionsFromJSON(options.publicKey);
            navigator.credentials[registration?'create':'get']({publicKey}).then(c=>done(c.toJSON())).catch(e=>done({error:String(e)}));""",options,registration)
    def verified(self, value):
        self.call('POST',self.path('/goog/cdp/execute'),{'cmd':'WebAuthn.setUserVerified','params':{'authenticatorId':self.authenticator,'isUserVerified':value}})
    def close(self):
        if self.session:
            logs=self.call('POST',self.path('/log'),{'type':'browser'})
            errors=[entry for entry in logs if entry['level']=='SEVERE' and '401' not in entry['message'] and '400' not in entry['message'] and '403' not in entry['message'] and '/favicon.ico' not in entry['message'] and '/system/readiness' not in entry['message']]
            if errors: raise AssertionError('Unexpected browser errors: '+json.dumps(errors))
            self.call('DELETE',self.path(''))
        self.driver.terminate();self.driver.wait(timeout=10);self.log.close()

browser=Browser()
try:
    # Authentication gates the deployed application, including before setup.
    browser.visit(ORIGIN+'/dashboard/')
    browser.wait("return document.body.innerText.includes('Set up piqueld')")
    assert browser.api('me')['status']==401
    assert browser.async_js("const done=arguments[arguments.length-1];fetch('/api/v1/applications').then(r=>done(r.status)).catch(e=>done(String(e)))")==401
    setup=pathlib.Path(os.environ['AUTH_TEST_SETUP']).read_text().strip()
    browser.visit(setup)
    browser.wait("return document.body.innerText.includes('Create your account')")
    browser.screenshot('onboarding.png')
    browser.fill('Username','alice')
    browser.click('Create account with a passkey')
    browser.wait("return location.pathname==='/dashboard/' && document.body.innerText.includes('Applications')")
    alice=browser.api('me')['body']
    assert alice['username']=='alice'
    assert browser.api('status')['body']['initialized'] is True
    assert browser.api('register/start',{'invitation':setup.split('#invite=')[1],'user_id':None,'username':'intruder','display_name':'','passkey_name':'test'})['status']==401
    # Browser session is HTTP-only. Sign-out revokes it; direct passkey login works.
    assert 'piqueld_session' not in browser.js('return document.cookie')
    browser.click('Sign out')
    browser.wait("return document.body.innerText.includes('Sign in with a passkey')")
    browser.click('Sign in with a passkey')
    browser.wait("return document.body.innerText.includes('Applications')")
    assert browser.api('me')['body']['id']==alice['id']
    # The newly merged application editor uses the same authenticated boundary.
    browser.click('+ Create application')
    browser.fill('Application name','auth-integration')
    browser.click('Create application')
    browser.wait("return location.pathname.includes('/dashboard/applications/') && document.body.innerText.includes('auth-integration')")
    print('PASS: passkey session can create an application through the new editor', flush=True)
    browser.visit(ORIGIN+'/dashboard/')
    browser.wait("return document.body.innerText.includes('Applications')")
    print('PASS: browser setup and username-less passkey login', flush=True)
    # A real signed assertion succeeds exactly once. Browser-supplied identity is
    # untrusted; changing its user handle must not authenticate another account.
    challenge=browser.api('login/start',{})['body']
    proof={'id':challenge['id'],'credential':browser.ceremony(challenge['options'])}
    assert browser.api('login/finish',proof)['status']==200
    assert browser.api('login/finish',proof)['status']==401
    challenge=browser.api('login/start',{})['body']
    credential=browser.ceremony(challenge['options'])
    credential['response']['userHandle']='bm90LWFuLWFjY291bnQ'
    assert browser.api('login/finish',{'id':challenge['id'],'credential':credential})['status']==401
    # Downgrading the browser request cannot downgrade the server's UV policy.
    challenge=browser.api('login/start',{})['body']
    challenge['options']['publicKey']['userVerification']='discouraged'
    browser.verified(False)
    credential=browser.ceremony(challenge['options'])
    browser.verified(True)
    assert 'error' not in credential,credential
    assert browser.api('login/finish',{'id':challenge['id'],'credential':credential})['status']==401
    print('PASS: assertion replay, wrong user handle, and user-verification downgrade rejected', flush=True)
    # Account-management UI and invitation registration by its first recipient.
    browser.visit(ORIGIN+'/dashboard/accounts')
    browser.wait("return document.body.innerText.includes('Create invitation')")
    browser.screenshot('accounts.png')
    browser.click('Create invitation')
    browser.wait("return document.querySelector('.auth-secret').textContent.includes('#invite=')")
    invitation=browser.js("return document.querySelector('.auth-secret').textContent.split('\\n').pop()")
    browser.visit(invitation)
    browser.wait("return document.body.innerText.includes('Create your account')")
    browser.fill('Username','bob')
    browser.click('Create account with a passkey')
    browser.wait("return location.pathname==='/dashboard/' && document.body.innerText.includes('Applications')")
    bob=browser.api('me')['body']
    assert bob['username']=='bob'
    assert browser.api('register/start',{'invitation':invitation.split('#invite=')[1],'user_id':None,'username':'late','display_name':'','passkey_name':'test'})['status']==401
    assert browser.api('manage',{'action':'update_user','user_id':alice['id'],'username':'alice-edited','display_name':'Edited by Bob'})['status']==200
    print('PASS: invitation signup and unrestricted account editing', flush=True)
    # Two independently bound browsers may start the same invitation, but only
    # one transaction can create an account from it.
    from concurrent.futures import ThreadPoolExecutor
    racing_link=browser.api('manage',{'action':'create_invitation'})['body']['invitation_url']
    recipients=[Browser(),Browser()]
    try:
        proofs=[]
        for index,recipient in enumerate(recipients):
            recipient.visit(ORIGIN+'/dashboard/auth')
            recipient.wait("return document.body.innerText.includes('Sign in with a passkey')")
            challenge=recipient.api('register/start',{'invitation':racing_link.split('#invite=')[1],'user_id':None,'username':f'racer{index}','display_name':'','passkey_name':'Race test'})['body']
            proofs.append({'id':challenge['id'],'credential':recipient.ceremony(challenge['options'],True)})
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures=[pool.submit(recipient.api,'register/finish',proof) for recipient,proof in zip(recipients,proofs)]
            results=[future.result() for future in futures]
        assert sorted(result['status'] for result in results)==[200,401],results
        winner=next(result['body'] for result in results if result['status']==200)
        assert browser.api('manage',{'action':'delete_user','user_id':winner['id']})['status']==200
    finally:
        for recipient in recipients: recipient.close()
    # Bob can enroll a fresh passkey for Alice without Alice's approval.
    # Use a second device: the original already contains Alice's passkey and is
    # correctly excluded by WebAuthn's duplicate-enrollment protection.
    browser.call('POST',browser.path('/goog/cdp/execute'),{'cmd':'WebAuthn.removeVirtualAuthenticator','params':{'authenticatorId':browser.authenticator}})
    browser.authenticator=browser.add_authenticator()
    challenge=browser.api('register/start',{'invitation':None,'user_id':alice['id'],'username':'','display_name':'','passkey_name':'Enrolled by Bob'})['body']
    assert browser.api('register/finish',{'id':challenge['id'],'credential':browser.ceremony(challenge['options'],True)})['status']==200
    assert browser.api('me')['body']['id']==bob['id']
    print('PASS: concurrent invitation redemption and cross-account passkey enrollment', flush=True)

    # Device flow through the actual CLI, storing a credential for the Unix socket.
    cli=ROOT/'target/debug/piquelctl'
    env=os.environ.copy();env.pop('PIQUELD_TOKEN',None)
    env['PIQUELD_CREDENTIALS_FILE']=str(pathlib.Path(os.environ['AUTH_TEST_DIR'])/'credentials.json')
    command=[str(cli),'--socket',os.environ['AUTH_TEST_SOCKET']]
    process=subprocess.Popen(command+['login'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=False,bufsize=0)
    code=None
    import select
    for _ in range(4):
        ready,_,_=select.select([process.stderr],[],[],15)
        assert ready, 'CLI did not print login instructions within 15 seconds'
        line=process.stderr.readline().decode()
        match=re.search(r'Enter code: ([A-Z2-9-]+)',line)
        if match: code=match.group(1);break
    assert code, 'CLI did not display a device code'
    browser.visit(ORIGIN+'/dashboard/auth#device')
    browser.wait("return document.body.innerText.includes('Connect piquelctl')")
    browser.fill('CLI code',code)
    browser.click('Approve CLI login')
    browser.wait("return document.body.innerText.includes('CLI approved')")
    stdout,stderr=process.communicate(timeout=20)
    assert process.returncode==0,stderr
    who=subprocess.run(command+['--json','whoami'],env=env,capture_output=True,text=True,check=True)
    assert json.loads(who.stdout)['id']==bob['id']
    assert pathlib.Path(env['PIQUELD_CREDENTIALS_FILE']).stat().st_mode & 0o777 == 0o600
    subprocess.run(command+['logout'],env=env,capture_output=True,text=True,check=True)
    assert subprocess.run(command+['whoami'],env=env,capture_output=True).returncode!=0
    # An automation token has full account-management access, including another account.
    issued=browser.api('manage',{'action':'create_token','user_id':bob['id'],'name':'test','days':None})
    assert issued['status']==200
    env['PIQUELD_TOKEN']=issued['body']['token']
    assert subprocess.run(command+['whoami'],env=env,capture_output=True).returncode==0
    browser.visit(ORIGIN+'/dashboard/accounts')
    browser.wait("return document.body.innerText.includes('alice-edited')")
    browser.click('Delete account')
    assert 'alice-edited' in browser.call('GET', browser.path('/alert/text'))
    browser.call('POST', browser.path('/alert/dismiss'), {})
    assert any(user['id']==alice['id'] for user in browser.api('directory')['body']['users'])
    browser.click('Delete account')
    browser.call('POST', browser.path('/alert/accept'), {})
    browser.wait("return !document.body.innerText.includes('alice-edited')")
    assert all(user['id']!=alice['id'] for user in browser.api('directory')['body']['users'])
    print('PASS: account deletion requires confirmation and cancellation preserves the account', flush=True)
    assert browser.api('manage',{'action':'delete_user','user_id':bob['id']})['status']==400
    assert browser.api('manage',{'action':'revoke_all','user_id':bob['id']})['status']==200
    assert subprocess.run(command+['whoami'],env=env,capture_output=True).returncode!=0
    assert browser.api('me')['status']==401
    print('PASS: real browser setup, discoverable passkeys, invitations, unrestricted account editing, device CLI login over Unix, private credential storage, API tokens, revocation and last-account safeguard')
finally:
    browser.close()
