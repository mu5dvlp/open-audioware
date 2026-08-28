using System.IO;
using UnityEditor;
using UnityEditor.Build.Reporting;
using UnityEngine;

namespace Measurement.EditorTools
{
    /// <summary>
    /// M1 A/B 計測用シーン(<see cref="MeasurementSceneBuilder"/> が組み立てる
    /// <c>Assets/Measurement/MeasurementScene.unity</c>)から Android の apk を書き出す
    /// (docs/measurement-m1.md §4.2)。iOS 側の <see cref="IosXcodeExporter"/> と対になる。
    /// <para>
    /// バッチモードから
    /// <c>-executeMethod Measurement.EditorTools.AndroidApkExporter.Build</c> で実行する想定。
    /// 署名は Unity 既定のデバッグ鍵のまま(実機への adb install はユーザー作業、
    /// 初期構築仕様 §11)。
    /// </para>
    /// </summary>
    public static class AndroidApkExporter
    {
        private const string SceneToBuild = "Assets/Measurement/MeasurementScene.unity";
        private const string OutputPath = "Build/Android/measurement.apk";

        /// <summary>
        /// Unity 既定の applicationIdentifier(<c>com.DefaultCompany.unity-sample</c>)は
        /// ハイフンを含むため Android のパッケージ名として不正でビルドが落ちる。計測用の暫定値。
        /// </summary>
        private const string ApplicationIdentifier = "com.mu5dvlp.measurement";

        /// <summary>
        /// UPM パッケージ側のネイティブプラグイン(<c>libmw_ffi.so</c>)のインポータ設定を
        /// Android / ARM64 に揃える。
        /// <para>
        /// <c>.so</c> はビルド成果物なのでコミットしない(CLAUDE.md)。そのため
        /// <c>.meta</c> にインポータ設定が入っておらず、そのままでは Unity が
        /// 「どのプラットフォーム向けのプラグインか」を判断できず apk に同梱されない
        /// (2026-08-22 に実際に踏んだ: <c>lib/arm64-v8a/</c> に libmw_ffi.so が入らず、
        /// B が動かない apk ができた)。毎ビルド冪等に設定し直して再発を防ぐ。
        /// </para>
        /// </summary>
        private static bool ConfigureNativePluginImporter()
        {
            const string pluginPath =
                "Packages/com.mu5dvlp.open-audioware/Runtime/Plugins/Android/libs/arm64-v8a/libmw_ffi.so";

            if (AssetImporter.GetAtPath(pluginPath) is not PluginImporter importer)
            {
                Debug.LogError(
                    $"AndroidApkExporter: native plugin not found or not a plugin: {pluginPath}" +
                    " (先に make build-android を実行すること)");
                return false;
            }

            importer.SetCompatibleWithAnyPlatform(false);
            importer.SetCompatibleWithPlatform(BuildTarget.Android, true);
            importer.SetPlatformData(BuildTarget.Android, "CPU", "ARM64");
            importer.SaveAndReimport();
            Debug.Log($"AndroidApkExporter: native plugin configured for Android/ARM64: {pluginPath}");
            return true;
        }

        [MenuItem("Measurement/Export Android APK")]
        public static void Build()
        {
            // メニューから実行された場合に備えて明示的に切り替える
            // (バッチモードでは Makefile が -buildTarget Android を渡すので通常は no-op)。
            EditorUserBuildSettings.SwitchActiveBuildTarget(BuildTargetGroup.Android, BuildTarget.Android);

            PlayerSettings.SetScriptingBackend(BuildTargetGroup.Android, ScriptingImplementation.IL2CPP);
            PlayerSettings.Android.targetArchitectures = AndroidArchitecture.ARM64;
            // ネイティブライブラリは cargo-ndk で API level 26 向けに作っている(Makefile の
            // ANDROID_API_LEVEL)。ここを下げると起動時にリンクが失敗しうるので揃えること。
            PlayerSettings.Android.minSdkVersion = AndroidSdkVersions.AndroidApiLevel26;
            PlayerSettings.SetApplicationIdentifier(BuildTargetGroup.Android, ApplicationIdentifier);
            // aab ではなく apk が欲しい(adb install で実機へ入れるため)。
            EditorUserBuildSettings.buildAppBundle = false;

            if (!ConfigureNativePluginImporter())
            {
                EditorApplication.Exit(1);
                return;
            }

            string projectRoot = Directory.GetParent(Application.dataPath)!.FullName;
            string outputPath = Path.Combine(projectRoot, OutputPath);
            Directory.CreateDirectory(Path.GetDirectoryName(outputPath)!);

            var options = new BuildPlayerOptions
            {
                scenes = new[] { SceneToBuild },
                locationPathName = outputPath,
                target = BuildTarget.Android,
                targetGroup = BuildTargetGroup.Android,
                options = BuildOptions.Development,
            };

            BuildReport report = BuildPipeline.BuildPlayer(options);
            if (report.summary.result != BuildResult.Succeeded)
            {
                Debug.LogError($"AndroidApkExporter: build failed ({report.summary.result}), totalErrors={report.summary.totalErrors}");
                EditorApplication.Exit(1);
                return;
            }

            Debug.Log($"AndroidApkExporter: apk written to {outputPath}");
        }
    }
}
