"""Verify mounted Solve setup (`solve_setup.js`), Explore shell (`idea.js`), and library send-back (`library_send_back.js`) in real headless Chromium."""

from pathlib import Path
import tempfile

from playwright.sync_api import expect
from recovery_example_browser import open_install


def check_mode_stages(browser, binary):
    with tempfile.TemporaryDirectory(prefix="re-search-mode-stages-") as directory:
        root = Path(directory)
        (root / "paper").mkdir()

        def mock_no_models(page):
            page.route("**/api/models*", lambda r: r.fulfill(json={"providers": []}))
            page.add_init_script("window.localStorage.setItem('re-search-explore-enabled', '1');")

        with open_install(browser, binary, root, before_navigate=mock_no_models) as page:
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            # 1. Left-rail Solve is enabled by default (no localStorage override), while Explore stays disabled with "not built yet" (F5).
            expect(page.locator("#new-solve-btn")).to_be_enabled()
            assert "not built yet" not in page.locator("#new-solve-btn").inner_text()
            expect(page.locator("#new-explore-btn")).to_be_disabled()
            expect(page.locator("#new-explore-btn")).to_contain_text("not built yet")

            # 2. Home Explore card (`new_menu.js` + `idea_setup.js`) renders when flag enabled
            #    and submitting opens `#explore-session-stage` (`idea.js`) immediately!
            home_explore = page.locator("#home-explore-card")
            expect(home_explore).to_be_visible()
            home_explore.locator("#idea-setup-text").fill(
                "Explore spectral gap scaling in coupled harmonic chains"
            )
            home_explore.locator("#idea-setup-offline").check()
            home_explore.locator("#idea-setup-submit").click()

            explore_stage = page.locator("#explore-session-stage")
            expect(explore_stage).to_be_visible()
            expect(home_explore).to_be_hidden()
            expect(explore_stage.locator('[data-idea-shell="true"]')).to_be_visible()
            expect(explore_stage.locator('[data-idea-section="stage-rail"]')).to_contain_text("Idea")
            expect(explore_stage.locator('[data-idea-section="stage-rail"]')).to_contain_text("Directions")
            expect(explore_stage.locator('[data-idea-section="idea-card"]')).to_contain_text(
                "Explore spectral gap scaling in coupled harmonic chains"
            )

            # 3. Click left-rail Solve directly (no reload/API workaround) ->
            #    `mode_stages.js` mounts `solve_setup.js`, `solve_steps.js`, and `solve_transcribe.js` in `#solve-session-stage`!
            page.locator("#new-solve-btn").click()

            solve_stage = page.locator("#solve-session-stage")
            expect(solve_stage).to_be_visible()
            expect(explore_stage).to_be_hidden()
            expect(solve_stage.locator('[data-testid="solve-setup-card"]')).to_be_visible()
            expect(solve_stage.locator('[data-testid="solve-workspace"]')).to_be_visible()
            transcribe_tab = solve_stage.locator('[data-tab-id="transcribe"]')
            expect(transcribe_tab).to_be_visible()
            transcribe_tab.click()
            expect(solve_stage.locator('[data-testid="solve-transcribe-list"]')).to_be_visible()
            expect(solve_stage.locator('[data-testid="solve-approval-host"]')).to_contain_text(
                "Approvals for this Solve run"
            )
            expect(solve_stage).to_contain_text("Numerically checked")
            assert "Checked numerically" not in solve_stage.inner_text()

            stmt = solve_stage.locator('[data-testid="solve-statement-input"]')
            stmt.fill(r"Compute $V_1(\phi)$ for $\lambda > 0$.")
            expect(solve_stage.locator('[data-testid="solve-katex-preview"] .katex').first).to_be_visible()
            solve_stage.locator('[data-testid="solve-conventions-input"]').fill("Euclidean signature, hbar = 1")
            solve_stage.locator('[data-testid="solve-run-btn"]').click()

            expect(solve_stage.locator('[data-testid="solve-collapsed-bar"]')).to_be_visible()
            expect(solve_stage.locator('[data-testid="solve-given-steps"]')).to_contain_text("Given")
            expect(solve_stage.locator('[data-testid="solve-given-steps"]')).to_contain_text("Euclidean signature")

            # Switch back to the Explore session in the rail -> swaps cleanly to `#explore-session-stage`!
            page.locator("#session-list .session-item").filter(
                has=page.locator('[data-mode-tag="explore"]')
            ).click()
            expect(explore_stage).to_be_visible()
            expect(solve_stage).to_be_hidden()

            # 4. Open Projects -> Sources tab -> add a paper -> verify `library_send_back.js` mounts!
            page.locator("#projects-open-btn").click()
            page.get_by_role("button", name="New project", exact=True).click()
            page.get_by_label("Project name").fill("Mode Stages Library Check")
            page.get_by_role("button", name="Save project").click()
            page.get_by_role("tab", name="Sources").click()
            page.locator("[data-add-paper]").click()
            form = page.locator(".project-library-form")
            form.locator('input[name="title"]').fill("Synthetic Damping Bound")
            form.locator('input[name="reference"]').fill("arXiv:2405.01234v1")
            form.locator('textarea[name="bibtex"]').fill(
                "@article{synth2024,\n  title={Synthetic Damping Bound}\n}"
            )
            form.locator('button[type="submit"]').click()

            send_back = page.locator(".project-paper-send-back .library-send-back-slot")
            expect(send_back).to_be_visible()
            expect(send_back.locator(".send-back-export-btn")).to_have_text("Export BibTeX")
            expect(send_back.locator(".send-back-kandinsky-btn")).to_have_text("Send to Kandinsky")
            expect(send_back).to_contain_text("synth2024")
            send_back.locator(".send-back-export-btn").click()
            expect(send_back.locator(".send-back-dialog")).to_be_visible()

            # 5. Investigate setup renders its "Investigate a paper" heading only once (no duplicate heading).
            page.locator("#new-investigation-btn").click()
            expect(page.locator("#investigation-setup")).to_be_visible()
            expect(page.get_by_role("heading", name="Investigate a paper", exact=True)).to_have_count(1)

            # 6. With two connected models, Solve's Solver dropdown lists both plus "Not selected",
            #    and clicking Run with "Ask before running" unchecked starts the first leg automatically.
            continue_calls = []
            page.unroute("**/api/models*")
            page.route(
                "**/api/models*",
                lambda r: r.fulfill(
                    json={
                        "providers": [
                            {
                                "id": "chatgpt",
                                "label": "ChatGPT",
                                "route": "api",
                                "models": [
                                    {"id": "gpt-6.2-sol", "label": "gpt-6.2-sol", "default": True},
                                    {"id": "gpt-5.4", "label": "gpt-5.4", "default": False},
                                ],
                            }
                        ]
                    }
                ),
            )
            page.route(
                "**/solve/continue",
                lambda r: (continue_calls.append(r.request.url), r.fulfill(status=202, json={"ok": True, "accepted": True})),
            )
            page.locator("#new-solve-btn").click()
            solver_opts = solve_stage.locator('[data-testid="solve-solver-select"] option')
            expect(solver_opts).to_have_count(3)
            expect(solver_opts.nth(0)).to_have_text("Not selected")
            expect(solver_opts.nth(1)).to_contain_text("gpt-6.2-sol")
            expect(solver_opts.nth(2)).to_contain_text("gpt-5.4")
            solver_opts_select = solve_stage.locator('[data-testid="solve-solver-select"]')
            solver_opts_select.select_option("chatgpt::gpt-6.2-sol")
            solve_stage.locator('[data-testid="solve-statement-input"]').fill(r"Compute $\int_0^1 x^2\,dx$.")
            with page.expect_response("**/solve/continue"):
                solve_stage.locator('[data-testid="solve-run-btn"]').click()
            assert len(continue_calls) == 1, continue_calls

            assert not errors, errors
    print("Mode stages (Solve setup, Explore shell, and Library send-back) browser check passed")
