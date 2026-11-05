import { connect, Uncertain } from "/usr/local/lib/elsebot/cdp/cdp.mjs";

const [url, query] = process.argv.slice(2);
const browser = await connect();
try {
  const page = await browser.page({ create: url });
  await page.fill("input[name=q]", query);
  try {
    await page.click("button[type=submit]");
  } catch (e) {
    if (e instanceof Uncertain) {
      console.error("the click may or may not have happened:", e.message);
    }
    throw e;
  }
  await page.waitFor("document.querySelector('#results')", { timeoutMs: 10000 });
  const [title, results] = await Promise.all([
    page.read("document.title"),
    page.read("document.querySelector('#results').innerText"),
  ]);
  console.log(JSON.stringify({ title, results }));
} finally {
  await browser.close();
}
