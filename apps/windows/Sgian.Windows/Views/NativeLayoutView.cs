using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Sgian.Protocol;
using Windows.System;

namespace Sgian.Windows.Views;

internal static class NativeLayoutView
{
    public static FrameworkElement Build(PaneLayout node, Func<string, FrameworkElement> pane,
        Action<string, double> saveRatio)
    {
        if (node.IsLeaf) return pane(node.Id);
        var horizontal = node.Direction == "row";
        var grid = new Grid();
        var ratio = node.Ratio;
        var divider = new Thumb { MinWidth = 7, MinHeight = 7, IsTabStop = true };
        AutomationProperties.SetName(divider, horizontal ? "Resize columns with arrow keys" : "Resize rows with arrow keys");
        if (horizontal)
        {
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(ratio, GridUnitType.Star) });
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(7) });
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1 - ratio, GridUnitType.Star) });
        }
        else
        {
            grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(ratio, GridUnitType.Star) });
            grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(7) });
            grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1 - ratio, GridUnitType.Star) });
        }
        void Resize(double next)
        {
            ratio = PaneLayout.Clamp(next);
            if (horizontal)
            {
                grid.ColumnDefinitions[0].Width = new GridLength(ratio, GridUnitType.Star);
                grid.ColumnDefinitions[2].Width = new GridLength(1 - ratio, GridUnitType.Star);
            }
            else
            {
                grid.RowDefinitions[0].Height = new GridLength(ratio, GridUnitType.Star);
                grid.RowDefinitions[2].Height = new GridLength(1 - ratio, GridUnitType.Star);
            }
            saveRatio(node.Id, ratio);
        }
        divider.DragDelta += (_, args) => Resize(ratio + (horizontal ? args.HorizontalChange : args.VerticalChange)
            / Math.Max(1, (horizontal ? grid.ActualWidth : grid.ActualHeight) - 7));
        divider.KeyDown += (_, args) =>
        {
            if (args.Key is VirtualKey.Left or VirtualKey.Up) { Resize(ratio - 0.05); args.Handled = true; }
            if (args.Key is VirtualKey.Right or VirtualKey.Down) { Resize(ratio + 0.05); args.Handled = true; }
        };
        var first = Build(node.First!, pane, saveRatio);
        var second = Build(node.Second!, pane, saveRatio);
        // Cached pane frames may previously have occupied the other axis or
        // the second slot. Reset both attached positions when reparenting.
        Grid.SetColumn(first, 0); Grid.SetRow(first, 0);
        Grid.SetColumn(second, horizontal ? 2 : 0); Grid.SetRow(second, horizontal ? 0 : 2);
        Grid.SetColumn(divider, horizontal ? 1 : 0); Grid.SetRow(divider, horizontal ? 0 : 1);
        grid.Children.Add(first); grid.Children.Add(divider); grid.Children.Add(second);
        return grid;
    }
}
