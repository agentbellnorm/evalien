# SEB, a Swedish bank

Sign in with `await connections.connect("seb")`: the human approves with Mobile BankID on their phone. The session stays alive while you use it, and ends after an hour unused. Then `fetch` the bank's APIs directly. Cookies are attached for you, and responses are JSON unless noted.

## Who the customer is

The bank's customer ID isn't served by an API; it's in the home page's HTML:

```js
const html = await (await fetch("https://apps.seb.se/cps/home")).text();
const user = JSON.parse(html.match(/window\.\$\$Shell\.user = window\.\$\$Shell\.user \|\| (\{.*\})/)[1]);
// user.id (same as user.sebCustNo) is the userId the APIs below take.
// user.name, and user.friendlyId: the personal identity number.
```

## Accounts and balances

`GET https://apps.seb.se/ssc/accounts-widget/api/accounts/all?userId={id}`

```
{ data: { active_accounts: [{
    id, iban, bban, custom_name, currency_code,
    account_type: { code, name },
    balance, disposable_amount, approved_credit_amount, reserved_amount: { amount, currency_code },
    primary_account_owner: { name, personal_identity_number }
  }], status_code } }
```

Amounts are strings. `disposable_amount` is what can be spent; `balance` is the booked balance.

## Also available

- `GET https://apps.seb.se/ssc/accounts-widget/api/user-preference?userId={id}`: `{ isAvailableBalance, availableBalance }`, whether the customer prefers to see the disposable or the booked balance.
- `GET https://apps.seb.se/cps/api/notifications/private`: `{ result: { total_count, prioritized: { title, events }, informative: { title, events } } }`, messages from the bank.

## Limits

- Read only. Payments and transfers need BankID signing, which isn't available.
- A 401, 403 or a redirect to id.seb.se means the session has ended: connect again.
- Transactions, cards and loans aren't mapped yet. To find them, look at which APIs the web app at https://apps.seb.se/cps/home loads, and keep what you learn in your database.
