using System.IO;
using UnityEditor;
using UnityEditor.Build.Reporting;
using UnityEngine;

namespace Measurement.EditorTools
{
    /// <summary>
    /// M1 A/B 計測用シーン(<see cref="MeasurementSceneBuilder"/> が組み立てる
    /// <c>Assets/Measurement/MeasurementScene.unity</c>)から iOS 用 Xcode プロジェクトを
    /// 書き出す(docs/measurement-m1.md §4.1 手順3-4)。
    /// <para>
    /// バッチモードから
    /// <c>-executeMethod Measurement.EditorTools.IosXcodeExporter.Build</c> で実行する想定。
    /// 署名は「自動署名(Automatically manage signing)」を有効にした状態で書き出すのみ
    /// — 実際の Team 選択・実機インストールは Xcode 上でのユーザー作業(初期構築仕様 §11)。
    /// </para>
    /// </summary>
    public static class IosXcodeExporter
    {
        private const string SceneToBuild = "Assets/Measurement/MeasurementScene.unity";
        private const string OutputDirName = "Build/iOS";

        [MenuItem("Measurement/Export iOS Xcode Project")]
        public static void Build()
        {
            PlayerSettings.SetScriptingBackend(BuildTargetGroup.iOS, ScriptingImplementation.IL2CPP);
            PlayerSettings.SetArchitecture(BuildTargetGroup.iOS, 1); // ARM64
            PlayerSettings.iOS.appleEnableAutomaticSigning = true;
            PlayerSettings.iOS.targetOSVersionString = "15.0";

            string projectRoot = Directory.GetParent(Application.dataPath)!.FullName;
            string outputPath = Path.Combine(projectRoot, OutputDirName);
            Directory.CreateDirectory(outputPath);

            var options = new BuildPlayerOptions
            {
                scenes = new[] { SceneToBuild },
                locationPathName = outputPath,
                target = BuildTarget.iOS,
                targetGroup = BuildTargetGroup.iOS,
                options = BuildOptions.Development,
            };

            BuildReport report = BuildPipeline.BuildPlayer(options);
            if (report.summary.result != BuildResult.Succeeded)
            {
                Debug.LogError($"IosXcodeExporter: build failed ({report.summary.result}), totalErrors={report.summary.totalErrors}");
                EditorApplication.Exit(1);
                return;
            }

            Debug.Log($"IosXcodeExporter: Xcode project written to {outputPath}");
        }
    }
}
