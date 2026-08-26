import os, sys, importlib.util
# 保证默认代理
os.environ.pop("AIS_SWITCH_PROXY", None)
_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("setup_ais_codex", os.path.join(_HERE, "..", "setup-ais-codex.py"))
m = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(m)

fixtures = os.path.join(_HERE, "fixtures")
before = open(os.path.join(fixtures, "settings-before.yaml")).read()
cred_before = open(os.path.join(fixtures, "credentials-before.yaml")).read()

models = [
    {"id": "llm-gateway--deepseek-v4-flash", "name": "deepseek-v4-flash"},
    {"id": "llm-gateway--glm-5.2", "name": "glm-5.2"},
]

# after-setup: upsert provider + 保留其它 provider + set-default
after_setup = m.upsert_ais_codex_provider(before, models)
after_setup = m.upsert_default_model(after_setup, m.pick_default_model(models))
cred_after_setup = m.upsert_credential(cred_before)

# after-remove: 移除 provider + 还原默认模型 + 清凭据
after_remove = m.restore_default_model(m.remove_ais_codex_provider(after_setup))
cred_after_remove = m.remove_credential(cred_after_setup)

open(os.path.join(fixtures, "settings-after-setup.yaml"), "w").write(after_setup)
open(os.path.join(fixtures, "credentials-after-setup.yaml"), "w").write(cred_after_setup)
open(os.path.join(fixtures, "settings-after-remove.yaml"), "w").write(after_remove)
open(os.path.join(fixtures, "credentials-after-remove.yaml"), "w").write(cred_after_remove)

print("--- settings-after-setup ---")
print(after_setup)
print("--- credentials-after-setup ---")
print(cred_after_setup)
print("--- settings-after-remove ---")
print(after_remove)
print("--- credentials-after-remove ---")
print(cred_after_remove)
