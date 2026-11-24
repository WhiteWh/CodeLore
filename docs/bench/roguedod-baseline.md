# RogueDOD / UE control baseline - SocratiCode vs. CodeLore Atlas

Historical baseline measured 2026-11-18T11:36:36.004Z on Windows 10.0.26200 x64. [Raw results](roguedod-baseline.json).

## Tools and corpora

- lore: `lore 0.4.1 (9de6304)`, Atlas text search (`--text-only`).
- SocratiCode: `1.16.1` via MCP `codebase_search` (hybrid BM25 + dense, RRF), server already up.
- Node v24.15.0; Qdrant 1.19.1 (managed, http://127.0.0.1:16333); Ollama ollama/nomic-embed-text.
- RogueDOD revision: `3bcd05a2d444aa1d91869a50a9317918a4a0ff3e` (develop); dirty: AGENTS.md, RogueDOD/.gitignore, RogueDOD/.mcp.json.
- Unreal Engine 5.8.4 (CL 58902052, ++UE5+Release-5.8).
- RogueDOD Atlas: 6277 files, 40816 pieces, 43923 symbols, 13934 import edges, 0 vectors (text-only run; semantic configured but not embedded). The SQLite index was read from a private cache copy.
  - extensions: cpp 2621, h 2609, md 408, py 345, ush 84, Build.cs 83, usf 74, ps1 17, sh 16, hlsl 15; docs 413, C++ 5230, plugin files 3020.
  - `Plugins/VaCuus/**` indexed: **no** (Only 104 paths contain 'VaCuus' and all are the game's integrations/docs (Source/RogueDOD/.../UI/VaCuus, Docs/VaCuus); Plugins/VaCuus/** is excluded by RogueDOD/.atlasignore (third-party plugin whitelist does not re-include it).).
- RogueDOD SocratiCode: 5157/5157 files, 28978 chunks, indexed 2026-11-07T14:08:33.481Z, built by 1.16.2, queried with 1.16.1; embedding ollama/nomic-embed-text (768d, `search_query: `).
  - unique paths in payload: 5154; top dirs: Source 2751, Balance 1221, Plugins 855, Docs 272, Tools 22, Config 19. Plugins: RoguePCG 460, RogueArena 149, RogueBalance 119, ProjectQueryServer 65, RogueCoreUtility 62.
  - `Plugins/VaCuus/**` indexed: **no** (Plugins/VaCuus/** is absent from the collection; the only VaCuus hits are Source/RogueDOD/.../UI/VaCuus, Docs/VaCuus and Tools/VaCuusBake.).
- UE SocratiCode: 63034/63037 files, 388711 chunks, indexed 2026-09-02T23:24:51.589Z. CoreMinimal.h is present (2 chunks), ActorComponent.cpp 159 chunks, Actor.cpp 287 chunks.
  - Index built 2026-09-02; Runtime/Engine/Private/Components/ActorComponent.cpp was last written 2026-10-22, so the index may not match the current tree byte for byte. Content checks (the SetComponentTickEnabled key) still land at the current line numbers.
- UE Atlas was outside this baseline. Engine queries were measured using SocratiCode.

Latency: wall clock of one call, 3 repetitions, minimum kept; the SocratiCode server is warm (one untimed search before the run), each Atlas call is a fresh `lore` process. Ranking: every repetition is scored; the table reports the best expected rank over them and the JSON keeps all repetitions (`rank_reps`), because Qdrant's fused order moves by a rank between identical runs.

Scoring follows docs/BENCHMARK.md: a result *answers* when its text holds one of the query's keys (exact, case-sensitive substring); file-location queries answer on the expected path. *Expected* is the first result on the expected path (and line, when given), with alternates counted. The VaCuus-plugin probe is excluded from the totals because neither corpus indexes `Plugins/VaCuus`.

## Reproduce

Use the [benchmark setup](../../tools/bench/README.md#run) with local paths
and a copy of the prepared cache.

Index metadata: `hgs socraticode status <path> --json`; Qdrant at `http://127.0.0.1:16333` (native, 1.19.1); Atlas counts from `lore atlas status --json`. The shared index and the RogueDOD working tree were not modified; only read-only searches ran.

## Results

| query | corpus | kind | SOC expected | SOC answer | SOC ms | Atlas expected | Atlas answer | Atlas ms |
|---|---|---|---|---|---|---|---|---|
| `rd-equip-exact` | roguedod | exact | 1 | 1 | 87 | 2 | 2 | 1035 |
| `rd-equip-nl` | roguedod | natural-language | 5 | 5 | 54 | - | - | 1056 |
| `rd-equip-edge` | roguedod | static-call | 1 | 1 | 41 | 3 | 3 | 988 |
| `rd-weapon-switch-exact` | roguedod | exact | 1 | 1 | 56 | 2 | 2 | 981 |
| `rd-vacuus-exact` | roguedod | exact | 1 | 1 | 49 | 2 | 2 | 1229 |
| `rd-vacuus-nl` | roguedod | natural-language | 1 | 1 | 49 | 4 | 4 | 1084 |
| `rd-vacuus-plugin-gap` | roguedod | exact (gap probe) | - | - | 60 | - | - | 1040 |
| `ue-coreminimal-exact` | ue | exact | - | - | 175 | n/a | n/a | - |
| `ue-actorcomponent-file` | ue | exact | 1 | 1 | 177 | n/a | n/a | - |
| `ue-actorcomponent-nl` | ue | natural-language | - | - | 176 | n/a | n/a | - |
| `ue-register-edge` | ue | static-call | 3 | 3 | 175 | n/a | n/a | - |
| `ue-coreminimal-nl` | ue | natural-language | - | - | 175 | n/a | n/a | - |
| `ue-tick-exact` | ue | exact | 5 | 3 | 167 | n/a | n/a | - |

## Summary (measured queries only)

| corpus/system | queries | expected@1 | expected@3 | expected@10 | answered@10 | median ms |
|---|---|---|---|---|---|---|
| roguedod/socraticode | 6 | 5 | 5 | 6 | 6 | 51.5 |
| roguedod/atlas | 6 | 0 | 4 | 5 | 5 | 1045.5 |
| ue/socraticode | 6 | 1 | 2 | 3 | 3 | 175 |

## Top-10

### rd-equip-exact - `UInventoryWeapon::Equip`

Expected: `Source/RogueDOD/Private/Items/InventoryWeapon.cpp`:28.

**socraticode** (87 ms):

1. `Source\RogueDOD\Private\Items\InventoryWeapon.cpp:1-51` score 0.5
2. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Types\Bitset.h:182-185` score 0.5
3. `Source\RogueDOD\Private\Components\RogueSpriteMarkerComponent.cpp:305-317` score 0.3333
4. `Source\RogueDOD\Public\Items\InventoryWeapon.h:1-27` score 0.3333
5. `Source\RogueDOD\Public\Map\RogueFastTravelSubsystem.h:15-28` score 0.25
6. `Source\RogueDOD\Private\Items\ItemsDataRow.cpp:1-36` score 0.25
7. `Plugins\RoguePCG\Source\RoguePCGEditor\Private\AssetTypeActions\PCGPointBufferTypeActions.cpp:83-93` score 0.2
8. `Balance\schema.json:15931-16030` score 0.2
9. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Utility\FunctionInvocationHelper.h:769-770` score 0.1667
10. `Balance\tables\Presets\DT_EnemyDataPresets\Pusher.Hammer.Default.json:1-100` score 0.1667

**atlas** (1035 ms):

1. `RogueDOD/Source/RogueDOD/Public/Items/InventoryWeapon.h:1-27` score 0.04892
2. `RogueDOD/Source/RogueDOD/Private/Items/InventoryWeapon.cpp:28-50` score 0.01639
3. `RogueDOD/Source/RogueDOD/Private/Items/InventoryWeapon.cpp:1-27` score 0.01587
4. `RogueDOD/Source/RogueDOD/Private/Items/InventoryRangeWeaponItem.cpp:27-49` score 0.01563
5. `RogueDOD/Source/RogueDOD/Public/Items/InventoryRangeWeaponItem.h:1-28` score 0.01538
6. `RogueDOD/Docs/Guides/WeaponAuthoringGuide.md:121-180` score 0.01515
7. `RogueDOD/Docs/Guides/WeaponAuthoringGuide.md:361-369` score 0.01493
8. `RogueDOD/Docs/Guides/EnemyAuthoringGuide.md:1-60` score 0.01471
9. `RogueDOD/Source/RogueDOD/Private/UI/Weapon/RogueWeaponPanelLibrary.cpp:1-18` score 0.01449
10. `RogueDOD/Source/RogueDOD/Public/Equipment/EquipmentData.h:1-25` score 0.01429

### rd-equip-nl - `where equipping a weapon item hands the weapon data to the owner's weapon component and initializes its weapon actors`

Expected: `Source/RogueDOD/Private/Items/InventoryWeapon.cpp`:28.

**socraticode** (54 ms):

1. `Balance\tables\Items\DT_EnemyWeaponsItems\AI.HeadBomb.json:1-100` score 0.5
2. `Source\RogueDOD\Private\Weapons\WeaponComponent.cpp:236-256` score 0.5
3. `Source\RogueDOD\Private\Weapons\WeaponComponent.cpp:54-80` score 0.3333
4. `Balance\tables\Items\DT_PlayerWeaponsItems\PairSwords.json:91-190` score 0.3333
5. `Source\RogueDOD\Private\Items\InventoryWeapon.cpp:1-51` score 0.25
6. `Balance\tables\Items\DT_EnemyWeaponsItems\AI.HeadBomb.Elite.json:1-100` score 0.25
7. `Docs\KeySubsystems.md:66-68` score 0.2
8. `Config\DefaultGameplayTags.ini:1-100` score 0.2
9. `Balance\tables\Items\DT_EnemyWeaponsItems\AI.Sword.json:91-190` score 0.1667
10. `Source\RogueDOD\Private\AbilitySystem\Abilities\ModularAbility\AbilityModules\LaunchComponentAbilityModule.cpp:209-218` score 0.1667

**atlas** (1056 ms):

1. `RogueDOD/Docs/Guides/EnemyAuthoringGuide.md:1-60` score 0.01639
2. `RogueDOD/Docs/Guides/WeaponAuthoringGuide.md:1-60` score 0.01613
3. `RogueDOD/Source/RogueDOD/Public/Equipment/EquipmentRuntimeData.h:71-157` score 0.01587
4. `RogueDOD/Docs/Guides/WeaponAuthoringGuide.md:301-360` score 0.01563
5. `RogueDOD/Source/RogueDOD/Private/Equipment/EquipmentRuntimeData.cpp:110-137` score 0.01538
6. `RogueDOD/Source/RogueDOD/Private/Equipment/EquipmentProcessorComponent.cpp:453-466` score 0.01429
7. `RogueDOD/Source/RogueDOD/Private/Weapons/MeleeWeaponTraceComponent.cpp:274-299` score 0.01408
8. `RogueDOD/Docs/Guides/EnemyAuthoringGuide.md:601-619` score 0.01389
9. `RogueDOD/Source/RogueDOD/Public/Equipment/EquipmentData.h:1-25` score 0.0137
10. `RogueDOD/Source/RogueDOD/Private/Weapons/EquipmentWeapon.cpp:1-23` score 0.01333

### rd-equip-edge - `which UWeaponComponent functions does UInventoryWeapon::Equip call after Super::Equip`

Expected: `Source/RogueDOD/Private/Items/InventoryWeapon.cpp`:28.
Call edges: `Source/RogueDOD/Private/Items/InventoryWeapon.cpp:30` -> `Source/RogueDOD/Private/Items/InventoryItem.cpp:89` (Super::Equip -> UInventoryItem::Equip); `Source/RogueDOD/Private/Items/InventoryWeapon.cpp:34` -> `Source/RogueDOD/Private/Weapons/WeaponComponent.cpp:207` (OwningWeaponComponent->SwitchWeapon -> UWeaponComponent::SwitchWeapon); `Source/RogueDOD/Private/Items/InventoryWeapon.cpp:35` -> `Source/RogueDOD/Private/Weapons/WeaponComponent.cpp:237` (OwningWeaponComponent->InitializeWeaponActors -> UWeaponComponent::InitializeWeaponActors).

**socraticode** (41 ms):

1. `Source\RogueDOD\Private\Items\InventoryWeapon.cpp:1-51` score 0.5
2. `Source\RogueDOD\Private\Interaction\Terminal\RogueTerminalActor.cpp:103-113` score 0.5
3. `Source\RogueDOD\Private\UI\Settings\RogueSettingDataSources.cpp:346-358` score 0.3333
4. `Source\RogueDOD\Public\Items\InventoryWeapon.h:1-27` score 0.3333
5. `Source\RogueDOD\Private\Settings\RogueUpscaleApplier.cpp:189-203` score 0.25
6. `Source\RogueDOD\Private\Items\InventoryRangeWeaponItem.cpp:1-50` score 0.25
7. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Private\RogueCoreUtility.cpp:1-20` score 0.2333
8. `Source\RogueDOD\Public\AI_V2\Navigation\Tactical\RogueTacticalNavSubsystem.h:221-238` score 0.2
9. `Source\RogueDOD\Private\Weapons\WeaponComponent.cpp:171-180` score 0.2
10. `Source\RogueDOD\Private\Subsystems\EquipmentSubsystem.cpp:1-92` score 0.1667

**atlas** (988 ms):

1. `RogueDOD/Source/RogueDOD/Public/Items/InventoryWeapon.h:1-27` score 0.04918
2. `RogueDOD/Source/RogueDOD/Public/Weapons/WeaponComponent.h:69-148` score 0.04152
3. `RogueDOD/Source/RogueDOD/Private/Items/InventoryWeapon.cpp:28-50` score 0.01613
4. `RogueDOD/Source/RogueDOD/Private/Items/InventoryWeapon.cpp:1-27` score 0.01587
5. `RogueDOD/Source/RogueDOD/Public/Items/InventoryRangeWeaponItem.h:1-28` score 0.01563
6. `RogueDOD/Source/RogueDOD/Private/Weapons/WeaponComponent.cpp:435-459` score 0.01538
7. `RogueDOD/Source/RogueDOD/Private/Items/InventoryRangeWeaponItem.cpp:27-49` score 0.01515
8. `RogueDOD/Source/RogueDOD/Private/Weapons/WeaponComponent.cpp:82-171` score 0.01493
9. `RogueDOD/Source/RogueDOD/Private/Items/InventoryRangeWeaponItem.cpp:1-26` score 0.01429
10. `RogueDOD/Source/RogueDOD/Private/Weapons/BaseWeapon.cpp:84-110` score 0.01205

### rd-weapon-switch-exact - `UWeaponComponent::SwitchWeapon`

Expected: `Source/RogueDOD/Private/Weapons/WeaponComponent.cpp`:207.

**socraticode** (56 ms):

1. `Source\RogueDOD\Private\Weapons\WeaponComponent.cpp:206-235` score 0.5
2. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Types\Bitset.h:182-185` score 0.5
3. `Source\RogueDOD\Private\Components\RogueSpriteMarkerComponent.cpp:305-317` score 0.3333
4. `Source\RogueDOD\Private\Items\InventoryWeapon.cpp:1-51` score 0.3333
5. `Source\RogueDOD\Private\Weapons\WeaponComponent.cpp:181-205` score 0.25
6. `Source\RogueDOD\Public\Map\RogueFastTravelSubsystem.h:15-28` score 0.25
7. `Source\RogueDOD\Public\Weapons\WeaponComponent.h:65-164` score 0.2
8. `Plugins\RoguePCG\Source\RoguePCGEditor\Private\AssetTypeActions\PCGPointBufferTypeActions.cpp:83-93` score 0.2
9. `Balance\tables\Abilities\DT_EnemyAbilities\AI.SwitchWeapon.json:1-81` score 0.1667
10. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Utility\FunctionInvocationHelper.h:769-770` score 0.1667

**atlas** (981 ms):

1. `RogueDOD/Source/RogueDOD/Public/Weapons/WeaponComponent.h:69-148` score 0.04817
2. `RogueDOD/Source/RogueDOD/Private/Weapons/WeaponComponent.cpp:207-226` score 0.01639
3. `RogueDOD/Source/RogueDOD/Private/Weapons/WeaponComponent.cpp:172-206` score 0.01613
4. `RogueDOD/Source/RogueDOD/Private/Weapons/MobWeaponComponent.cpp:124-168` score 0.01587
5. `RogueDOD/Source/RogueDOD/Private/AbilitySystem/Abilities/WeaponSwitchGameplayAbility.cpp:1-26` score 0.0102
6. `RogueDOD/Source/RogueDOD/Public/Weapons/WeaponComponent.h:285-295` score 0.0098
7. `RogueDOD/Source/RogueDOD/Private/AbilitySystem/Abilities/ActivateRangeAttackAbility.cpp:36-43` score 0.00971
8. `RogueDOD/Source/RogueDOD/Private/Helpers/WeaponHelper.cpp:32-47` score 0.00962
9. `RogueDOD/Source/RogueDOD/Private/Items/InventoryWeapon.cpp:28-50` score 0.00952
10. `RogueDOD/Source/RogueDOD/Public/Components/Interfaces/WeaponComponentOwnerInterface.h:17-28` score 0.00935

### rd-vacuus-exact - `URogueVaCuusScreen::LoadDocument`

Expected: `Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp`:308.
Call edges: `Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:318` -> `Plugins/VaCuus/Source/VaCuus/Private/VaCuusView.cpp:166` (View->LoadDocument -> UVaCuusView::LoadDocument).

**socraticode** (49 ms):

1. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusScreen.cpp:307-331` score 0.5
2. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Types\Bitset.h:182-185` score 0.5
3. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusScreen.cpp:140-171` score 0.3333
4. `Source\RogueDOD\Private\Components\RogueSpriteMarkerComponent.cpp:305-317` score 0.3333
5. `Source\RogueDOD\Public\Map\RogueFastTravelSubsystem.h:15-28` score 0.25
6. `Docs\VaCuus\reports\13-runtime.md:91-190` score 0.25
7. `Docs\VaCuus\reports\20-settings-doc.md:631-651` score 0.2
8. `Plugins\RoguePCG\Source\RoguePCGEditor\Private\AssetTypeActions\PCGPointBufferTypeActions.cpp:83-93` score 0.2
9. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Utility\FunctionInvocationHelper.h:769-770` score 0.1667
10. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusScreen.h:111-195` score 0.1667

**atlas** (1229 ms):

1. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusScreen.h:65-144` score 0.04866
2. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:298-327` score 0.01639
3. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:141-172` score 0.01613
4. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusConsole.cpp:140-170` score 0.01515
5. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusItemsPresenter.cpp:158-175` score 0.01493
6. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusNotesPresenter.cpp:115-132` score 0.01471
7. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusPresenter.h:60-139` score 0.01429
8. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusConsole.cpp:106-139` score 0.01351
9. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusPresenter.h:1-59` score 0.01299
10. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusPauseScreen.cpp:165-228` score 0.0125

### rd-vacuus-nl - `how a Rogue VaCuus screen swaps its document on the existing view while keeping bound models alive`

Expected: `Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp`:308.

**socraticode** (49 ms):

1. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusScreen.h:111-195` score 0.75
2. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusPauseSettings.h:13-112` score 0.5
3. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusPauseScreen.h:13-56` score 0.4333
4. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusPresenter.h:15-114` score 0.3833
5. `Docs\VaCuus\agents\vacuus-screen-bind.md:1-100` score 0.375
6. `Docs\VaCuus\agents\vacuus-runtime.md:91-119` score 0.254
7. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusPresenter.cpp:8-24` score 0.24
8. `Docs\VaCuus\agents\vacuus-runtime.md:1-100` score 0.2
9. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusScreen.cpp:140-171` score 0.1714
10. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusPauseScreen.cpp:169-227` score 0.1667

**atlas** (1084 ms):

1. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusPauseSettings.h:1-19` score 0.01639
2. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:141-172` score 0.01613
3. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusPauseScreen.cpp:165-228` score 0.01587
4. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:298-327` score 0.01563
5. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusItemsPresenter.cpp:158-175` score 0.01538
6. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusNotesPresenter.cpp:115-132` score 0.01515
7. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusScreen.h:65-144` score 0.01493
8. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusPausePresenter.cpp:240-288` score 0.01471
9. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusPauseScreen.h:1-46` score 0.01449
10. `RogueDOD/Docs/VaCuus/agents/vacuus-runtime.md:1-60` score 0.01429

### rd-vacuus-plugin-gap - `UVaCuusView::LoadDocument`

Expected: `Plugins/VaCuus/Source/VaCuus/Private/VaCuusView.cpp`:166.

**socraticode** (60 ms):

1. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Types\Bitset.h:182-185` score 0.5
2. `Docs\VaCuus\reports\13-runtime.md:91-190` score 0.5
3. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusScreen.h:111-195` score 0.3333
4. `Source\RogueDOD\Private\Components\RogueSpriteMarkerComponent.cpp:305-317` score 0.3333
5. `Source\RogueDOD\Public\Map\RogueFastTravelSubsystem.h:15-28` score 0.25
6. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusScreen.cpp:307-331` score 0.25
7. `Source\RogueDOD\Public\UI\VaCuus\RogueVaCuusScreen.h:14-20` score 0.2
8. `Plugins\RoguePCG\Source\RoguePCGEditor\Private\AssetTypeActions\PCGPointBufferTypeActions.cpp:83-93` score 0.2
9. `Plugins\RogueCoreUtility\Source\RogueCoreUtility\Public\Utility\FunctionInvocationHelper.h:769-770` score 0.1667
10. `Source\RogueDOD\Private\UI\VaCuus\RogueVaCuusScreen.cpp:140-171` score 0.1667

**atlas** (1040 ms):

1. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:298-327` score 0.01639
2. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusScreen.cpp:141-172` score 0.01613
3. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusItemsPresenter.cpp:158-175` score 0.01587
4. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusNotesPresenter.cpp:115-132` score 0.01563
5. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusPresenter.h:60-139` score 0.01515
6. `RogueDOD/Source/RogueDOD/Public/UI/VaCuus/RogueVaCuusScreen.h:65-144` score 0.01493
7. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusItemsScreen.cpp:98-109` score 0.01471
8. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusNotesScreen.cpp:99-110` score 0.01449
9. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusUnlocksScreen.cpp:104-115` score 0.01429
10. `RogueDOD/Source/RogueDOD/Private/UI/VaCuus/RogueVaCuusPauseScreen.cpp:152-164` score 0.01408

### ue-coreminimal-exact - `CoreMinimal.h`

Expected: `Runtime/Core/Public/CoreMinimal.h`:1.

**socraticode** (175 ms):

1. `Runtime\Apple\MetalRHI\Public\MetalThirdParty.h:1-17` score 0.5
2. `Runtime\GeometryCore\Public\DynamicMesh\MeshIndexUtil.h:176-183` score 0.5
3. `Runtime\MessagingRpc\Public\IMessageRpcClient.h:246-250` score 0.3333
4. `Developer\MassEntityTestSuite\Public\MassEntityTestFarmPlot.h:1-19` score 0.3333
5. `Runtime\Engine\Public\SkeletalMeshDeformerHelpers.h:1-8` score 0.25
6. `Runtime\LevelSequence\Public\LevelSequenceDirector.h:148-150` score 0.25
7. `Runtime\RenderCore\Public\RHIStaticStates.h:348-350` score 0.2
8. `Developer\LauncherServices\Public\ILauncherWorker.h:1-8` score 0.2
9. `Runtime\Engine\Public\AnimInterpFilter.h:1-12` score 0.1667
10. `Runtime\Core\Public\Containers\IntrusiveDoubleLinkedList.h:315-318` score 0.1667

**atlas**:

- unavailable: no Atlas index for this corpus; Atlas has no cross-repository search

### ue-actorcomponent-file - `ActorComponent.cpp`

Expected: `Runtime/Engine/Private/Components/ActorComponent.cpp`:1.

**socraticode** (177 ms):

1. `Runtime\Engine\Private\Components\ActorComponent.cpp:3422-3453` score 0.5
2. `Runtime\Engine\Private\Rendering\StaticLightingSystemInterface.cpp:10-18` score 0.5
3. `Editor\SequenceRecorder\Private\ActorRecording.cpp:182-203` score 0.3333
4. `Runtime\Engine\Private\Streaming\Texture2DMipDataProvider_DDC.cpp:17-26` score 0.3333
5. `Editor\BlueprintGraph\Private\K2Node_VariableGet.cpp:124-129` score 0.25
6. `Editor\MovieSceneTools\Private\TrackEditors\CustomPrimitiveDataTrackEditor.cpp:483-534` score 0.25
7. `Editor\SequenceRecorder\Private\ActorRecording.cpp:977-1106` score 0.2
8. `Runtime\Core\Private\Internationalization\ICUTextCharacterIterator.cpp:64-72` score 0.2
9. `Runtime\Engine\Private\Streaming\VolumeTextureStreaming.cpp:15-23` score 0.1667
10. `Developer\MeshUtilities\Private\MeshUtilities.cpp:5855-5886` score 0.1667

**atlas**:

- unavailable: no Atlas index for this corpus; Atlas has no cross-repository search

### ue-actorcomponent-nl - `where an actor component registers its tick functions when it is added to a world`

Expected: `Runtime/Engine/Private/Components/ActorComponent.cpp`:2038.

**socraticode** (176 ms):

1. `Runtime\Engine\Private\Actor.cpp:1347-1446` score 0.6429
2. `Runtime\Engine\Private\Actor.cpp:6185-6311` score 0.5
3. `Runtime\Engine\Classes\GameFramework\Actor.h:874-973` score 0.3333
4. `Runtime\Engine\Private\WorldPartition\ActorPartition\PartitionActorDesc.cpp:76-87` score 0.3333
5. `Runtime\Engine\Classes\Components\ActorComponent.h:986-1050` score 0.2526
6. `Runtime\Engine\Classes\GameFramework\Actor.h:784-883` score 0.25
7. `Editor\Kismet\Private\SCSEditorViewportClient.cpp:218-263` score 0.25
8. `Runtime\Engine\Classes\GameFramework\Actor.h:2854-2953` score 0.2222
9. `Runtime\AnimGraphRuntime\Private\BoneControllers\AnimNode_TwoBoneIK.cpp:260-278` score 0.2143
10. `Runtime\Engine\Classes\Engine\World.h:1235-1317` score 0.2095

**atlas**:

- unavailable: no Atlas index for this corpus; Atlas has no cross-repository search

### ue-register-edge - `who calls UActorComponent::RegisterComponentWithWorld while an actor registers all its components`

Expected: `Runtime/Engine/Private/Actor.cpp`:6262.
Call edges: `Runtime/Engine/Private/Actor.cpp:6262` -> `Runtime/Engine/Private/Components/ActorComponent.cpp:1967` (Component->RegisterComponentWithWorld -> UActorComponent::RegisterComponentWithWorld).

**socraticode** (175 ms):

1. `Runtime\Engine\Private\Components\ActorComponent.cpp:1966-2077` score 0.5
2. `Runtime\Engine\Classes\GameFramework\Actor.h:2854-2953` score 0.5
3. `Runtime\Engine\Private\Actor.cpp:6185-6311` score 0.3333
4. `Runtime\Engine\Classes\GameFramework\Actor.h:784-883` score 0.3333
5. `Runtime\Engine\Private\Actor.cpp:6100-6184` score 0.25
6. `Runtime\Engine\Classes\GameFramework\Actor.h:874-973` score 0.25
7. `Runtime\Engine\Public\Net\Iris\ReplicationSystem\EngineReplicationBridge.h:203-296` score 0.2111
8. `Runtime\NavigationSystem\Public\NavigationSystem.h:942-971` score 0.2
9. `Runtime\Engine\Private\Tests\AutoRTFM\AutoRTFMActorComponentTests.cpp:211-310` score 0.2
10. `Runtime\Engine\Private\DemoNetDriver.cpp:5248-5266` score 0.1667

**atlas**:

- unavailable: no Atlas index for this corpus; Atlas has no cross-repository search

### ue-coreminimal-nl - `the umbrella header that brings Unreal's core types, containers, logging and platform macros into a translation unit`

Expected: `Runtime/Core/Public/CoreMinimal.h`:1.

**socraticode** (175 ms):

1. `Runtime\CoreUObject\Public\UObject\LinkerLoad.h:1291-1388` score 0.5
2. `ThirdParty\Boost\Deploy\boost-1.88.0\include\boost\log\support\spirit_classic.hpp:1-52` score 0.5
3. `Editor\UnrealEd\Private\Commandlets\GatherTextFromSourceCommandlet.cpp:1486-1558` score 0.3333
4. `ThirdParty\libFLAC\flac-1.4.3\include\all.h:181-280` score 0.3333
5. `ThirdParty\FreeType2\FreeType2-2.14.1\include\freetype\config\public-macros.h:91-139` score 0.25
6. `ThirdParty\abseil\20240722.0\include\absl\flags\declare.h:1-69` score 0.25
7. `ThirdParty\WebRTC\5414\Include\third_party\abseil-cpp\absl\flags\declare.h:1-74` score 0.2
8. `Editor\UnrealEd\Private\Cooker\DiffWriterZenHeader.h:1-100` score 0.2
9. `Runtime\Core\Public\Windows\PreWindowsApi.h:1-45` score 0.1667
10. `ThirdParty\ICU\icu4c-64_1\include\unicode\ures.h:1-53` score 0.1667

**atlas**:

- unavailable: no Atlas index for this corpus; Atlas has no cross-repository search

### ue-tick-exact - `UActorComponent::SetComponentTickEnabled`

Expected: `Runtime/Engine/Private/Components/ActorComponent.cpp`:1755.
Call edges: `Runtime/Engine/Private/Components/ActorComponent.cpp:1759` -> `Runtime/Engine/Classes/Engine/EngineBaseTypes.h:364` (PrimaryComponentTick.SetTickFunctionEnable -> FActorComponentTickFunction::SetTickFunctionEnable).

**socraticode** (167 ms):

1. `Runtime\Engine\Private\Components\ActorComponent.cpp:3067-3086` score 0.5
2. `Runtime\Engine\Public\AudioDevice.h:212-234` score 0.5
3. `Runtime\Engine\Private\Components\ActorComponent.cpp:1762-1798` score 0.3333
4. `Runtime\Core\Public\Async\Future.h:37-39` score 0.3333
5. `Runtime\Engine\Private\Components\ActorComponent.cpp:1754-1761` score 0.25
6. `Runtime\Core\Public\Misc\FrameValue.h:36-38` score 0.25
7. `Runtime\CoreUObject\Public\UObject\DynamicallyTypedValue.h:285-289` score 0.2
8. `Runtime\Engine\Private\Components\ActorComponent.cpp:2823-2833` score 0.2
9. `Editor\Sequencer\Public\AnimatedPropertyKey.h:93-101` score 0.1667
10. `Runtime\Engine\Private\Components\ActorComponent.cpp:2812-2822` score 0.1667

**atlas**:

- unavailable: no Atlas index for this corpus; Atlas has no cross-repository search
