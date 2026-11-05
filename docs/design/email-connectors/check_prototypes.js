(() => {
  const expect = (value, message) => { if (!value) throw new Error(message); };
  const click = action => {
    const el = document.querySelector(`[data-action="${action}"]`);
    expect(el && !el.disabled, `Missing or disabled action ${action}`);
    el.click();
  };
  const option = (name, enabled) => {
    const el = document.querySelector(`[data-option="${name}"]`);
    expect(el, `Missing option ${name}`);
    el.checked = enabled;
    el.dispatchEvent(new Event('change', {bubbles:true}));
  };
  for (const provider of providers) {
    state.stage = 'choose';
    render();
    document.querySelector(`[data-provider="${provider.id}"]`).click();
    click('next');
    expect(state.stage === 'auth', `${provider.id} auth step`);
    const secret = document.getElementById('password');
    if (secret) secret.value = 'fictional-private-value';
    click('authorize');
    expect(state.stage === 'access', `${provider.id} access step`);
    expect(!secret || secret.value === '', 'Submitted password input was not cleared');
    expect(!JSON.stringify(state).includes('fictional-private-value'), 'Secret leaked into persistent prototype state');
    option('send', true);
    option('read', false);
    expect(document.querySelector('[data-action="activate"]').disabled, 'Wake must require read access');
    option('read', true);
    expect(!document.getElementById('scope'), 'Unexpected watched-folder selector');
    click('activate');
    expect(state.stage === 'done' && state.send, 'Access selections lost');
    click('mail');
    expect(document.getElementById('activity').innerText.includes('Duplicate notification ignored'), 'Missing event demo');
    click('pause');
    expect(document.querySelector('[data-action="mail"]').disabled, 'Paused listener accepted demo mail');
    click('pause');
    click('failure');
    expect(document.querySelector('[data-action="mail"]').disabled, 'Expired credentials accepted demo mail');
    click('reconnect');
    expect(state.stage === 'auth', 'Reconnect did not return to sign-in');
    expect(!document.getElementById('password')?.value, 'Reconnect retained a password');
    Object.assign(state, {read:true,wake:true,paused:false,error:false});
  }
  Object.assign(state,{stage:'access',provider:'gmail',address:'hello@studio.example',send:false,read:true,wake:true});
  render();
  expect(document.documentElement.scrollWidth <= window.innerWidth, 'Horizontal overflow');
  for (const button of document.querySelectorAll('button:not(:disabled)')) {
    expect(getComputedStyle(button).cursor === 'pointer', 'Enabled button lacks pointer cursor');
  }
  console.log(JSON.stringify({result:'PASS',variant,providers:providers.length,width:window.innerWidth,checks:'auth,secret clearing,permissions,wake dependency,no folder filter,event demo,pause,reconnect,overflow,cursors'}));
})();
