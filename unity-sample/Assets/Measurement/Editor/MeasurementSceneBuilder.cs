using System.IO;
using Measurement;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.EventSystems;
using UnityEngine.SceneManagement;
using UnityEngine.UI;

namespace Measurement.EditorTools
{
    /// <summary>
    /// M1 A/B 計測用シーン(docs/measurement-m1.md)をコードから組み立てて
    /// <see cref="ScenePath"/> に保存する。手作業の Unity エディタ操作に依存せず
    /// 再現可能にするため、GameObject 階層はすべてこのスクリプトで生成する。
    /// <para>
    /// バッチモードから <c>-executeMethod Measurement.EditorTools.MeasurementSceneBuilder.Build</c>
    /// で実行する想定(いかなる Unity 起動も CLAUDE.md のロック手順に従うこと)。
    /// エディタ上からは Measurement/Build M1 Scene メニューからも実行できる。
    /// </para>
    /// </summary>
    public static class MeasurementSceneBuilder
    {
        private const string SceneDir = "Assets/Measurement";
        private const string ScenePath = SceneDir + "/MeasurementScene.unity";

        [MenuItem("Measurement/Build M1 Scene")]
        public static void Build()
        {
            Scene scene = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);

            new GameObject("EventSystem", typeof(EventSystem), typeof(StandaloneInputModule));

            var canvasGo = new GameObject("Canvas", typeof(Canvas), typeof(CanvasScaler), typeof(GraphicRaycaster));
            var canvas = canvasGo.GetComponent<Canvas>();
            canvas.renderMode = RenderMode.ScreenSpaceOverlay;
            var scaler = canvasGo.GetComponent<CanvasScaler>();
            scaler.uiScaleMode = CanvasScaler.ScaleMode.ScaleWithScreenSize;
            scaler.referenceResolution = new Vector2(1080f, 1920f);

            Button buttonA = CreateBigButton(
                canvasGo.transform,
                "ButtonA",
                "A: Unity\nAudioSource.PlayOneShot",
                anchorMin: new Vector2(0f, 0f),
                anchorMax: new Vector2(0.5f, 0.85f),
                color: new Color(0.15f, 0.35f, 0.75f));

            Button buttonB = CreateBigButton(
                canvasGo.transform,
                "ButtonB",
                "B: Native\nMw.Native.MwNative.PlaySe",
                anchorMin: new Vector2(0.5f, 0f),
                anchorMax: new Vector2(1f, 0.85f),
                color: new Color(0.75f, 0.25f, 0.15f));

            Text statusText = CreateStatusText(canvasGo.transform);
            Image flashImage = CreateFlashOverlay(canvasGo.transform);

            var controllerGo = new GameObject("MeasurementController", typeof(AudioSource), typeof(MeasurementController));
            var controller = controllerGo.GetComponent<MeasurementController>();
            controller.screenFlash = flashImage.GetComponent<ScreenFlash>();
            controller.statusText = statusText;
            controller.unityAudioSource = controllerGo.GetComponent<AudioSource>();
            controller.buttonA = buttonA;
            controller.buttonB = buttonB;

            Directory.CreateDirectory(SceneDir);
            bool saved = EditorSceneManager.SaveScene(scene, ScenePath);
            AssetDatabase.SaveAssets();
            AssetDatabase.Refresh();

            EditorBuildSettings.scenes = new[] { new EditorBuildSettingsScene(ScenePath, true) };

            Debug.Log(saved
                ? $"MeasurementSceneBuilder: saved {ScenePath} and set as the sole Build Settings scene."
                : $"MeasurementSceneBuilder: FAILED to save {ScenePath}");
        }

        private static Button CreateBigButton(Transform parent, string name, string label, Vector2 anchorMin, Vector2 anchorMax, Color color)
        {
            var go = new GameObject(name, typeof(Image), typeof(Button));
            go.transform.SetParent(parent, false);

            var rect = go.GetComponent<RectTransform>();
            rect.anchorMin = anchorMin;
            rect.anchorMax = anchorMax;
            rect.offsetMin = new Vector2(8f, 8f);
            rect.offsetMax = new Vector2(-8f, -8f);

            go.GetComponent<Image>().color = color;
            Button button = go.GetComponent<Button>();

            var textGo = new GameObject("Label", typeof(Text));
            textGo.transform.SetParent(go.transform, false);
            var textRect = textGo.GetComponent<RectTransform>();
            textRect.anchorMin = Vector2.zero;
            textRect.anchorMax = Vector2.one;
            textRect.offsetMin = Vector2.zero;
            textRect.offsetMax = Vector2.zero;

            var text = textGo.GetComponent<Text>();
            text.text = label;
            text.alignment = TextAnchor.MiddleCenter;
            text.fontSize = 48;
            text.color = Color.white;
            text.font = Resources.GetBuiltinResource<Font>("LegacyRuntime.ttf");

            return button;
        }

        private static Text CreateStatusText(Transform parent)
        {
            var go = new GameObject("StatusText", typeof(Text));
            go.transform.SetParent(parent, false);

            var rect = go.GetComponent<RectTransform>();
            rect.anchorMin = new Vector2(0f, 0.85f);
            rect.anchorMax = new Vector2(1f, 1f);
            rect.offsetMin = Vector2.zero;
            rect.offsetMax = Vector2.zero;

            var text = go.GetComponent<Text>();
            text.text = "Mode: - (未タップ / not tapped yet)";
            text.alignment = TextAnchor.MiddleCenter;
            text.fontSize = 40;
            // 背景は黒(カメラのクリア色)なので黒文字だと録画で読めない。
            // 計測後に A/B のどちらを押していたかを動画から確認できることが重要なため白にする
            // (docs/measurement-m1.md §7.6-2)。
            text.color = Color.white;
            text.font = Resources.GetBuiltinResource<Font>("LegacyRuntime.ttf");

            return text;
        }

        private static Image CreateFlashOverlay(Transform parent)
        {
            // ボタン・ステータス表示の後に生成することで、GameObject 生成順 = sibling index の並びで
            // 最後(= uGUI の描画順で最前面)に来る。タップ中もクリックを妨げないよう
            // raycastTarget は無効にする。
            var go = new GameObject("FlashOverlay", typeof(Image), typeof(ScreenFlash));
            go.transform.SetParent(parent, false);

            var rect = go.GetComponent<RectTransform>();
            rect.anchorMin = Vector2.zero;
            rect.anchorMax = Vector2.one;
            rect.offsetMin = Vector2.zero;
            rect.offsetMax = Vector2.zero;

            var image = go.GetComponent<Image>();
            image.color = new Color(1f, 1f, 1f, 0f);
            image.raycastTarget = false;

            go.transform.SetAsLastSibling();
            return image;
        }
    }
}
